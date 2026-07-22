//! Server-switch latency bench. Talks to the already-running `geph manager` over
//! its local control pipe (which the VPN never touches), performs a series of
//! exit switches exactly the way the GUI does (`apply_settings` with a new
//! `exit_constraint`), and times each one. A background thread probes real
//! internet reachability (TCP to 1.1.1.1:443) so we capture the actual
//! user-visible outage, not just the control-plane timing.
//!
//! All output goes to Z:\switch-bench.log (and stdout), so it survives this
//! process's own network dropping during each switch.
//!
//! Run (elevated, while the manager is connected in VPN mode):
//!     cargo run --release --example switch_bench -p geph5-app

use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    net::{SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use geph5_broker_protocol::ExitConstraint;
use geph5_misc_rpc::manager_control::{
    self, ExitInfo, SessionContext, Status, TunnelSettings,
};
use isocountry::CountryCode;

const LOG_PATH: &str = r"Z:\switch-bench.log";
const NUM_SWITCHES: usize = 6;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(90);
const PROBE_TARGET: &str = "1.1.1.1:443";

/// Shared timeline logger: `[t+SSSS.mmm] message`, to file + stdout.
#[derive(Clone)]
struct Log {
    start: Instant,
    file: Arc<Mutex<File>>,
}

impl Log {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            start: Instant::now(),
            file: Arc::new(Mutex::new(File::create(LOG_PATH)?)),
        })
    }
    fn line(&self, msg: impl AsRef<str>) {
        let t = self.start.elapsed().as_secs_f64();
        let s = format!("[{t:8.3}] {}", msg.as_ref());
        println!("{s}");
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{s}");
            let _ = f.flush();
        }
    }
}

/// Flatten the nanorpc double-Result (transport error / app error) into anyhow.
async fn ctl<T, E: std::fmt::Debug>(
    fut: impl std::future::Future<Output = Result<Result<T, String>, E>>,
) -> anyhow::Result<T> {
    match fut.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(s)) => anyhow::bail!("manager returned error: {s}"),
        Err(e) => anyhow::bail!("transport error reaching manager: {e:?}"),
    }
}

fn constraint_label(c: &ExitConstraint) -> String {
    match c {
        ExitConstraint::Auto => "Auto".into(),
        ExitConstraint::Direct(s) => format!("Direct({s})"),
        ExitConstraint::Hostname(s) => format!("Hostname({s})"),
        ExitConstraint::Country(cc) => format!("Country({})", cc.alpha2()),
        ExitConstraint::CountryCity(cc, city) => format!("{}/{}", cc.alpha2(), city),
    }
}

fn state_label(s: &Status) -> String {
    let exit = s
        .exit
        .as_ref()
        .map(|e| format!(" via {}/{}", e.country, e.city))
        .unwrap_or_default();
    format!("{:?}{exit}", s.state)
}

/// Background reachability probe: logs every UP<->DOWN transition. The gap
/// between a DOWN and the next UP is one switch's real internet outage.
fn spawn_probe(log: Log) {
    thread::spawn(move || {
        let addr: SocketAddr = PROBE_TARGET.parse().unwrap();
        let mut last: Option<bool> = None;
        loop {
            let ok = TcpStream::connect_timeout(&addr, Duration::from_millis(700)).is_ok();
            if last != Some(ok) {
                log.line(format!(
                    "    PROBE {}",
                    if ok { "UP  (internet reachable)" } else { "DOWN (internet blocked)" }
                ));
                last = Some(ok);
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn main() -> anyhow::Result<()> {
    let log = Log::new()?;
    log.line("=== geph server-switch latency bench ===");

    geph5_rt::block_on(async move {
        let client = manager_control::manager_control_client();

        // Baseline: settings + account + exit list.
        let view = ctl(client.get_settings()).await?;
        log.line(format!(
            "manager: logged_in={} connected={} vpn={} exit={}",
            view.logged_in,
            view.connected,
            view.vpn,
            constraint_label(&view.exit_constraint)
        ));
        if !view.connected || !view.vpn {
            log.line("WARNING: manager is not connected in VPN mode; switch timings will not reflect the VPN reconnect path.");
        }
        let base: TunnelSettings = view.tunnel_settings();

        let level = match ctl(client.account()).await {
            Ok(a) => {
                log.line(format!("account level: {}", a.level));
                a.level
            }
            Err(e) => {
                log.line(format!("account lookup failed ({e:#}); assuming all exits usable"));
                "plus".into()
            }
        };
        let free_only = level == "free";

        let exits = ctl(client.list_exits()).await?;
        // Distinct (country, city) exits the account may use.
        let mut seen = HashSet::new();
        let usable: Vec<ExitInfo> = exits
            .into_iter()
            .filter(|e| !free_only || e.allows_free)
            .filter(|e| seen.insert((e.country.clone(), e.city.clone())))
            .collect();
        log.line(format!("{} distinct usable exits", usable.len()));
        if usable.len() < 2 {
            anyhow::bail!("need at least 2 usable exits to switch between; got {}", usable.len());
        }

        // Start probing real reachability before we perturb anything.
        spawn_probe(log.clone());
        thread::sleep(Duration::from_secs(2));

        let mut prev_label = constraint_label(&base.exit_constraint);
        let mut durations = Vec::new();

        for i in 0..NUM_SWITCHES {
            // Pick the next distinct target, different from the previous constraint.
            let target = &usable[i % usable.len()];
            let cc = match CountryCode::for_alpha2(&target.country) {
                Ok(cc) => cc,
                Err(_) => {
                    log.line(format!("skip exit with bad country code {}", target.country));
                    continue;
                }
            };
            let constraint = ExitConstraint::CountryCity(cc, target.city.clone());
            let label = constraint_label(&constraint);
            if label == prev_label {
                continue;
            }

            log.line(format!(
                "----- switch {}/{}: {} -> {} (load {:.2}) -----",
                i + 1,
                NUM_SWITCHES,
                prev_label,
                label,
                target.load
            ));

            let mut settings = base.clone();
            settings.exit_constraint = constraint;

            let t0 = Instant::now();
            ctl(client.apply_settings(settings, SessionContext::default())).await?;
            let apply = t0.elapsed();
            log.line(format!(
                "    apply_settings returned after {:.3}s (reconcile: kill routes/DNS/firewall + spawn engine + control-ready)",
                apply.as_secs_f64()
            ));

            // Poll status until Connected (or timeout), logging state transitions.
            let mut last_state = String::new();
            let mut connected_at = None;
            loop {
                let st = ctl(client.status()).await?;
                let lbl = state_label(&st);
                if lbl != last_state {
                    log.line(format!("    status: {} (t+{:.3}s)", lbl, t0.elapsed().as_secs_f64()));
                    last_state = lbl;
                }
                if matches!(st.state, manager_control::ConnState::Connected) {
                    connected_at = Some(t0.elapsed());
                    break;
                }
                if t0.elapsed() > CONNECT_TIMEOUT {
                    log.line("    TIMEOUT waiting for Connected");
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }

            if let Some(c) = connected_at {
                log.line(format!(
                    "    => Connected {:.3}s after switch (apply {:.3}s + session {:.3}s)",
                    c.as_secs_f64(),
                    apply.as_secs_f64(),
                    (c.saturating_sub(apply)).as_secs_f64()
                ));
                durations.push((apply, c));
            }

            // Dump the engine/manager logs produced during this switch, to attribute
            // where the seconds went internally.
            match ctl(client.logs(40)).await {
                Ok(lines) => {
                    log.line("    --- manager/engine logs (tail) ---");
                    for l in lines {
                        log.line(format!("      | {}", l.trim_end()));
                    }
                }
                Err(e) => log.line(format!("    (could not fetch logs: {e:#})")),
            }

            prev_label = label;
            // Let it stabilize and let the probe register a clean UP before the next switch.
            thread::sleep(Duration::from_secs(4));
        }

        // Restore the original exit.
        log.line(format!("restoring original exit: {}", constraint_label(&base.exit_constraint)));
        let _ = ctl(client.apply_settings(base.clone(), SessionContext::default())).await;

        // Summary.
        log.line("=== summary ===");
        if durations.is_empty() {
            log.line("no completed switches measured");
        } else {
            let n = durations.len() as f64;
            let apply_avg = durations.iter().map(|(a, _)| a.as_secs_f64()).sum::<f64>() / n;
            let conn_avg = durations.iter().map(|(_, c)| c.as_secs_f64()).sum::<f64>() / n;
            let apply_max = durations.iter().map(|(a, _)| a.as_secs_f64()).fold(0.0, f64::max);
            let conn_max = durations.iter().map(|(_, c)| c.as_secs_f64()).fold(0.0, f64::max);
            log.line(format!("switches measured: {}", durations.len()));
            log.line(format!("apply_settings (control-plane reconcile): avg {apply_avg:.3}s  max {apply_max:.3}s"));
            log.line(format!("time to Connected (full):                 avg {conn_avg:.3}s  max {conn_max:.3}s"));
            log.line("(real internet-down windows are the PROBE DOWN->UP gaps above)");
        }
        log.line("=== done ===");
        anyhow::Ok(())
    })?;

    Ok(())
}
