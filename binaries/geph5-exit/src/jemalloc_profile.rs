//! Quiet production heap sampling. SIGUSR1 requests an extra dump; four slots
//! bound disk use across process restarts. Profiles contain stacks, not payloads.
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::{self, File},
    mem::size_of,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use serde_json::{Value, json};

unsafe extern "C" {
    fn mallctl(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    fn malloc_stats_print(
        callback: Option<extern "C" fn(*mut c_void, *const c_char)>,
        opaque: *mut c_void,
        opts: *const c_char,
    );
}

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_dump(_: c_int) {
    // Signal handlers must not allocate, log, or call jemalloc.
    REQUESTED.store(true, Ordering::Relaxed);
}

fn read<T: Copy + Default>(name: &CStr) -> anyhow::Result<T> {
    let mut value = T::default();
    let mut size = size_of::<T>();
    let error = unsafe {
        mallctl(
            name.as_ptr(),
            (&mut value as *mut T).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    ensure!(
        error == 0 && size == size_of::<T>(),
        "mallctl {name:?}: error {error}, size {size}"
    );
    Ok(value)
}

fn write<T>(name: &CStr, value: &mut T) -> anyhow::Result<()> {
    let error = unsafe {
        mallctl(
            name.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            (value as *mut T).cast(),
            size_of::<T>(),
        )
    };
    ensure!(error == 0, "mallctl {name:?}: error {error}");
    Ok(())
}

fn snapshot() -> anyhow::Result<Value> {
    write(c"epoch", &mut 1_u64)?;
    let mut stats = serde_json::Map::new();
    for field in [
        "allocated",
        "active",
        "resident",
        "metadata",
        "mapped",
        "retained",
    ] {
        let name = CString::new(format!("stats.{field}"))?;
        stats.insert(field.into(), json!(read::<usize>(&name)?));
    }
    let status = fs::read_to_string("/proc/self/status")?;
    let status_kib = |field: &str| -> Option<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(field))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let listeners = sillad_sosistab3::listener::global_listener_stats();
    Ok(json!({
        "timestamp_unix": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "pid": std::process::id(),
        "rss_kib": status_kib("VmRSS:"),
        "swap_kib": status_kib("VmSwap:"),
        "jemalloc_bytes": stats,
        "prof_active": read::<bool>(c"prof.active")?,
        "lg_prof_sample": read::<usize>(c"prof.lg_sample")?,
        "rate_limiter_entries": crate::session::RATE_LIMITER_CACHE.entry_count(),
        "proxy_task_count": crate::tasklimit::get_task_count(),
        "sosistab_listeners": listeners.live_listeners,
        "sosistab_queue_payload_bytes": listeners.queue_payload_bytes,
    }))
}

fn save_json(path: &Path, value: &Value) -> anyhow::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

struct Output {
    fd: c_int,
    error: c_int,
}

extern "C" fn stats_output(opaque: *mut c_void, message: *const c_char) {
    // Avoid allocating while jemalloc is printing its own statistics.
    let output = unsafe { &mut *opaque.cast::<Output>() };
    if output.error != 0 {
        return;
    }
    let mut remaining = unsafe { CStr::from_ptr(message) }.to_bytes();
    while !remaining.is_empty() {
        let written = unsafe { libc::write(output.fd, remaining.as_ptr().cast(), remaining.len()) };
        if written < 0 {
            let error = unsafe { *libc::__errno_location() };
            if error == libc::EINTR {
                continue;
            }
            output.error = error;
            break;
        }
        if written == 0 {
            output.error = libc::EIO;
            break;
        }
        remaining = &remaining[written as usize..];
    }
}

fn dump(directory: &Path, slot: usize) -> anyhow::Result<()> {
    let heap = directory.join(format!("heap-{slot}.heap"));
    let temporary = heap.with_extension("heap.tmp");
    let path = CString::new(temporary.as_os_str().as_bytes())?;
    write(c"prof.dump", &mut path.as_ptr())?;
    fs::rename(&temporary, &heap)?;

    let allocator = directory.join(format!("allocator-{slot}.json"));
    let temporary = allocator.with_extension("json.tmp");
    let file = File::create(&temporary)?;
    let mut output = Output {
        fd: file.as_raw_fd(),
        error: 0,
    };
    unsafe {
        malloc_stats_print(
            Some(stats_output),
            (&mut output as *mut Output).cast(),
            c"J".as_ptr(),
        );
    }
    ensure!(
        output.error == 0,
        "allocator statistics write failed: {}",
        output.error
    );
    drop(file);
    fs::rename(temporary, allocator)?;
    save_json(&directory.join(format!("heap-{slot}.json")), &snapshot()?)?;
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    let Some(directory) = std::env::var_os("GEPH5_EXIT_PROFILE_DIR") else {
        return Ok(());
    };
    ensure!(
        read::<bool>(c"config.prof")? && read::<bool>(c"config.stats")?,
        "jemalloc profiling/stats not compiled in"
    );
    ensure!(
        read::<bool>(c"opt.prof")? && read::<bool>(c"prof.active")?,
        "set MALLOC_CONF=prof:true,prof_active:true"
    );
    let directory = PathBuf::from(directory);
    fs::create_dir_all(&directory)?;
    save_json(&directory.join("stats.json"), &snapshot()?)?;
    ensure!(
        unsafe {
            libc::signal(
                libc::SIGUSR1,
                request_dump as *const () as libc::sighandler_t,
            )
        } != libc::SIG_ERR,
        "could not install SIGUSR1 profiling handler"
    );
    tracing::info!(directory = %directory.display(), "jemalloc heap profiling enabled; SIGUSR1 requests a dump");
    std::thread::Builder::new()
        .name("heap-profile".into())
        .spawn(move || {
            let mut next_stats = Instant::now() + Duration::from_secs(60);
            let mut next_dump = next_stats;
            let mut slot = 0;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = Instant::now();
                let requested = REQUESTED.swap(false, Ordering::Relaxed);
                if now >= next_dump || requested {
                    if let Err(error) = dump(&directory, slot) {
                        tracing::error!(%error, "jemalloc heap dump failed");
                    }
                    slot = (slot + 1) % 4;
                    next_dump = now + Duration::from_secs(300);
                }
                if now >= next_stats {
                    if let Err(error) = snapshot()
                        .and_then(|stats| save_json(&directory.join("stats.json"), &stats))
                    {
                        tracing::error!(%error, "jemalloc statistics snapshot failed");
                    }
                    next_stats = now + Duration::from_secs(60);
                }
            }
        })
        .context("could not start heap profiler")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "run with MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:20"]
    fn profiles_a_live_allocation_and_writes_allocator_statistics() {
        assert!(read::<bool>(c"prof.active").unwrap());
        let allocation = vec![0x5a_u8; 16 * 1024 * 1024];
        std::hint::black_box(&allocation);
        let directory =
            std::env::temp_dir().join(format!("geph5-heap-test-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        dump(&directory, 0).unwrap();
        let stats: Value =
            serde_json::from_slice(&fs::read(directory.join("heap-0.json")).unwrap()).unwrap();
        assert!(stats["jemalloc_bytes"]["allocated"].as_u64().unwrap() >= allocation.len() as u64);
        let allocator: Value =
            serde_json::from_slice(&fs::read(directory.join("allocator-0.json")).unwrap()).unwrap();
        assert!(
            allocator["jemalloc"]["stats"]["allocated"]
                .as_u64()
                .unwrap()
                >= allocation.len() as u64
        );
        let heap = fs::read_to_string(directory.join("heap-0.heap")).unwrap();
        assert!(heap.starts_with("heap_v2/"));
        assert!(heap.lines().any(|line| line.starts_with("@ ")));
        fs::remove_dir_all(directory).unwrap();
    }
}
