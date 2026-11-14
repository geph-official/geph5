use anyctx::AnyCtx;
use anyhow::{Context as _, bail};
use file_rotate::{ContentLimit, FileRotate, compression::Compression, suffix::AppendCount};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use stdcode::StdcodeSerializeExt;
use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};

#[cfg(target_os = "ios")]
use tracing_oslog::OsLogger;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    client::{Config, LoggingConfig},
    database::DATABASE,
};

const LOG_FILE_PREFIX: &str = "geph5-client";
const MAX_RECENT_LOG_LINES: usize = 10_000;
const MAX_RECENT_LOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONFIG_LOG_FILES: u32 = 1_000;
const LOG_QUEUE_CAPACITY: usize = 1_024;

// The subscriber is process-wide, so its worker must also outlive individual
// Client handles. Entrypoints explicitly drain it before exiting the process.
static LOG_WORKER: Mutex<Option<WorkerGuard>> = Mutex::new(None);
static LOG_DROPS: Mutex<Option<ErrorCounter>> = Mutex::new(None);
static LOG_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn rotating_file(path: &Path, config: &LoggingConfig) -> anyhow::Result<FileRotate<AppendCount>> {
    validate_logging_config(config)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).context("could not create log directory")?;
    // FileRotate's constructor does not return file-open errors. Preflight the
    // active file so an unwritable destination is reported during startup.
    let mut options = OpenOptions::new();
    options.read(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).context("could not open log file")?;
    let make_writer = || {
        FileRotate::new(
            path,
            AppendCount::new(config.max_file as usize - 1),
            ContentLimit::BytesSurpassed(config.max_file_size as usize),
            Compression::None,
            Some(options.clone()),
        )
    };
    let mut writer = make_writer();
    // Apply a reduced file-count setting immediately, including after restart.
    // FileRotate otherwise prunes only on the next rotation.
    let old_paths = writer.log_paths();
    let excess = old_paths.len().saturating_sub(config.max_file as usize - 1);
    if excess > 0 {
        drop(writer);
        for path in old_paths.iter().take(excess) {
            fs::remove_file(path).context("could not remove excess log file")?;
        }
        writer = make_writer();
    }
    Ok(writer)
}

fn rotated_path(path: &Path, index: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    name.into()
}

/// Number of file log records dropped because the bounded queue was full.
pub fn dropped_log_lines() -> usize {
    LOG_DROPS
        .lock()
        .as_ref()
        .map_or(0, ErrorCounter::dropped_lines)
}

/// Drain queued file logs on orderly shutdown. Idempotent; call before process::exit.
/// As with WorkerGuard, flushing is best effort and has a bounded wait.
pub fn shutdown_logging() {
    let guard = LOG_WORKER.lock().take();
    drop(guard);
}

fn default_log_dir(config: &Config) -> PathBuf {
    if let Some(path) = config.logging.log_dir.clone() {
        return path;
    }
    if let Some(cache) = config.cache.as_ref()
        && cache.as_os_str() != ":memory:"
    {
        return cache
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("geph5")
}

fn log_path(config: &Config) -> PathBuf {
    let name = config
        .cache
        .as_ref()
        .filter(|path| path.as_os_str() != ":memory:")
        .and_then(|path| path.file_name())
        .map(|stem| {
            let mut name = stem.to_os_string();
            name.push(".log");
            name
        })
        .unwrap_or_else(|| {
            let credential_hash =
                hex::encode(blake3::hash(&config.credentials.stdcode()).as_bytes());
            format!("{LOG_FILE_PREFIX}-{credential_hash}.log").into()
        });
    default_log_dir(config).join(name)
}

fn validate_logging_config(config: &LoggingConfig) -> anyhow::Result<()> {
    if config.max_file == 0 {
        bail!("logging.max_file must be at least 1");
    }
    if config.max_file > MAX_CONFIG_LOG_FILES {
        bail!("logging.max_file must be at most {MAX_CONFIG_LOG_FILES}");
    }
    if config.max_file_size == 0 {
        bail!("logging.max_file_size must be greater than 0");
    }
    usize::try_from(config.max_file_size)
        .context("logging.max_file_size exceeds this platform’s addressable size")?;
    config
        .level
        .parse::<tracing_subscriber::filter::LevelFilter>()
        .with_context(|| format!("invalid logging.level {:?}", config.level))?;
    Ok(())
}

async fn migrate_legacy_logs(ctx: &AnyCtx<Config>) -> Result<(), sqlx::Error> {
    const PENDING: &str = "legacy_logs_vacuum_pending";
    let mut txn = ctx.get(DATABASE).begin().await?;
    let has_logs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'logs'",
    )
    .fetch_one(&mut *txn)
    .await?;
    if has_logs > 0 {
        sqlx::query("INSERT OR REPLACE INTO misc (key, value) VALUES (?, X'01')")
            .bind(PENDING)
            .execute(&mut *txn)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS logs")
            .execute(&mut *txn)
            .await?;
    }
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM misc WHERE key = ?")
        .bind(PENDING)
        .fetch_one(&mut *txn)
        .await?;
    txn.commit().await?;
    if pending == 0 {
        return Ok(());
    }
    // Dropping the old table frees pages but does not shrink the existing file.
    // Keep the marker until compaction completes so an interrupted upgrade or
    // a full disk does not prevent retrying on the next start.
    let mut conn = ctx.get(DATABASE).acquire().await?;
    sqlx::query("VACUUM").execute(&mut *conn).await?;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM misc WHERE key = ?")
        .bind(PENDING)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn spawn_legacy_log_migration(ctx: AnyCtx<Config>) {
    geph5_rt::spawn(async move {
        if let Err(err) = migrate_legacy_logs(&ctx).await {
            eprintln!("could not migrate legacy SQLite logs: {err}");
        }
    })
    .detach();
}

/// Initialize stderr logging and, unless disabled, a rotating JSON file sink.
/// The first client owns the process-wide subscriber, as in the previous logger.
pub fn init_logging(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let config = &ctx.init().logging;
    validate_logging_config(config)?;
    let mut worker = LOG_WORKER.lock();
    if LOG_INITIALIZED.load(Ordering::Relaxed) {
        return Ok(());
    }

    // RUST_LOG can override the configured default for stderr. The file layer
    // also has its own filter, so it cannot bypass logging.level.
    let stderr_filter = EnvFilter::builder()
        .with_default_directive(format!("geph={}", config.level).parse()?)
        .from_env_lossy();
    let file_level = config
        .level
        .parse::<tracing_subscriber::filter::LevelFilter>()?;

    #[cfg(not(target_os = "ios"))]
    let stderr_layer = fmt::layer().compact().with_writer(std::io::stderr);
    #[cfg(target_os = "ios")]
    let stderr_layer = OsLogger::new("geph.io.daemon", "default");
    use tracing_subscriber::Layer;

    let mut guard = None;
    let mut drops = None;
    let file_layer = if config.disabled {
        None
    } else {
        let file = rotating_file(&log_path(ctx.init()), config)?;
        let (writer, worker_guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(LOG_QUEUE_CAPACITY)
            .lossy(true)
            .thread_name("geph5-log-writer")
            .finish(file);
        drops = Some(writer.error_counter());
        guard = Some(worker_guard);
        Some(
            fmt::layer()
                .json()
                .with_writer(writer)
                .with_filter(file_level),
        )
    };
    let result = tracing_subscriber::registry()
        .with(stderr_layer.with_filter(stderr_filter))
        .with(file_layer)
        .try_init();
    if let Err(err) = result {
        // An embedding application may already own the process-wide subscriber.
        // Do not mistake prior thread-local subscribers for global initialization.
        if tracing::dispatcher::has_been_set() {
            LOG_INITIALIZED.store(true, Ordering::Relaxed);
            return Ok(());
        }
        return Err(err.into());
    }
    *worker = guard;
    *LOG_DROPS.lock() = drops;
    LOG_INITIALIZED.store(true, Ordering::Relaxed);
    spawn_legacy_log_migration(ctx.clone());
    Ok(())
}

fn read_log_tail(base: &Path, max_file: u32) -> std::io::Result<String> {
    let mut lines = VecDeque::new();
    let mut remaining = MAX_RECENT_LOG_BYTES;
    // Read newest files first, seeking straight to their tail. Both the amount
    // read and the response are bounded even if an old file is unexpectedly huge.
    for index in 0..max_file.min(MAX_CONFIG_LOG_FILES) {
        if remaining == 0 || lines.len() == MAX_RECENT_LOG_LINES {
            break;
        }
        let path = if index == 0 {
            base.to_owned()
        } else {
            rotated_path(base, index)
        };
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let size = file.metadata()?.len();
        let start = size.saturating_sub(remaining as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(remaining as u64).read_to_end(&mut bytes)?;
        remaining -= bytes.len();
        // Skip partial records at either edge (including a concurrent append).
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let begin = if start > 0 {
            bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(end, |i| i + 1)
        } else {
            0
        };
        let content = String::from_utf8_lossy(&bytes[begin.min(end)..end]);
        for line in content.lines().rev().filter(|line| !line.is_empty()) {
            lines.push_front(line.to_owned());
            if lines.len() == MAX_RECENT_LOG_LINES {
                break;
            }
        }
    }
    Ok(lines.into_iter().collect::<Vec<_>>().join("\n"))
}

/// Read a bounded chronological tail from the active and rotated JSON log files.
/// Queued records may take a short time to become visible to this API.
pub async fn get_json_logs(ctx: &AnyCtx<Config>) -> String {
    if ctx.init().logging.disabled {
        return String::new();
    }
    let path = log_path(ctx.init());
    let max_file = ctx.init().logging.max_file;
    tokio::task::spawn_blocking(move || read_log_tail(&path, max_file))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tracing_subscriber::Layer;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("geph5-logging-test-{}", rand::random::<u64>()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn log(&self) -> PathBuf {
            self.0.join("client.log")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "exit_constraint": "auto", "cache": ":memory:", "dry_run": true,
            "logging": { "disabled": true }
        }))
        .unwrap()
    }

    #[test]
    fn background_writer_rotates_whole_json_records_and_drains_on_shutdown() {
        let dir = TestDir::new();
        let config = LoggingConfig {
            max_file: 3,
            max_file_size: 256,
            ..Default::default()
        };
        let file = rotating_file(&dir.log(), &config).unwrap();
        // Lossless mode makes this integration test deterministic. Production
        // uses lossy mode so the tunnel never blocks behind the log queue.
        let (writer, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(16)
            .lossy(false)
            .finish(file);
        let subscriber =
            tracing_subscriber::registry().with(fmt::layer().json().with_writer(writer));
        tracing::subscriber::with_default(subscriber, || {
            for sequence in 0..100 {
                tracing::info!(sequence, "test record");
            }
        });
        drop(guard);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3);
        let mut expected = None;
        let mut records = 0;
        for index in (0..3).rev() {
            let path = if index == 0 {
                dir.log()
            } else {
                rotated_path(&dir.log(), index)
            };
            let content = fs::read_to_string(path).unwrap();
            for line in content.lines() {
                let record: serde_json::Value = serde_json::from_str(line).unwrap();
                let sequence = record["fields"]["sequence"].as_u64().unwrap();
                if let Some(value) = expected {
                    assert_eq!(sequence, value);
                }
                expected = Some(sequence + 1);
                records += 1;
            }
        }
        assert!(records > 1 && records < 100);
        assert_eq!(expected, Some(100));
    }

    #[test]
    fn one_file_and_restart_with_smaller_retention_are_supported() {
        let dir = TestDir::new();
        let mut config = LoggingConfig {
            max_file: 4,
            max_file_size: 8,
            ..Default::default()
        };
        {
            let mut writer = rotating_file(&dir.log(), &config).unwrap();
            for _ in 0..20 {
                writer.write_all(b"record-1\n").unwrap();
            }
        }
        fs::write(dir.0.join("unrelated.log"), "keep").unwrap();
        config.max_file = 1;
        let mut writer = rotating_file(&dir.log(), &config).unwrap();
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        for _ in 0..20 {
            writer.write_all(b"record-2\n").unwrap();
        }
        drop(writer);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        assert!(fs::read_to_string(dir.log()).unwrap().contains("record-2"));
        assert_eq!(
            fs::read_to_string(dir.0.join("unrelated.log")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn oversized_records_survive_rotation_intact() {
        let dir = TestDir::new();
        let config = LoggingConfig {
            max_file: 2,
            max_file_size: 16,
            ..Default::default()
        };
        let mut writer = rotating_file(&dir.log(), &config).unwrap();
        let large = format!("{{\"message\":\"{}\"}}\n", "a".repeat(4096));
        writer.write_all(large.as_bytes()).unwrap();
        writer.write_all(b"{}\n").unwrap();
        drop(writer);
        assert_eq!(
            fs::read_to_string(rotated_path(&dir.log(), 1)).unwrap(),
            large
        );
        assert_eq!(fs::read_to_string(dir.log()).unwrap(), "{}\n");
    }

    #[test]
    fn file_level_filters_events_before_enqueueing() {
        let dir = TestDir::new();
        let file = rotating_file(&dir.log(), &LoggingConfig::default()).unwrap();
        let (writer, guard) = NonBlockingBuilder::default().finish(file);
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .json()
                .with_writer(writer)
                .with_filter(tracing::level_filters::LevelFilter::WARN),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("not persisted");
            tracing::warn!("persisted");
        });
        drop(guard);
        let content = fs::read_to_string(dir.log()).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("persisted"));
        assert!(!content.contains("not persisted"));
    }

    #[test]
    fn recent_logs_are_chronological_and_bounded() {
        let dir = TestDir::new();
        fs::write(rotated_path(&dir.log(), 1), "old\nolder-tail\n").unwrap();
        fs::write(dir.log(), "new\npartial").unwrap();
        assert_eq!(
            read_log_tail(&dir.log(), 2).unwrap(),
            "old\nolder-tail\nnew"
        );
        let content = (0..MAX_RECENT_LOG_LINES + 20)
            .map(|i| format!("{i}\n"))
            .collect::<String>();
        fs::write(dir.log(), content).unwrap();
        let tail = read_log_tail(&dir.log(), 2).unwrap();
        assert_eq!(tail.lines().count(), MAX_RECENT_LOG_LINES);
        assert_eq!(tail.lines().next(), Some("20"));
        let huge = format!("{}\nlast\n", "a".repeat(MAX_RECENT_LOG_BYTES + 100));
        fs::write(dir.log(), huge).unwrap();
        assert_eq!(read_log_tail(&dir.log(), 2).unwrap(), "last");
    }

    #[tokio::test]
    async fn disabled_logs_have_an_empty_control_response() {
        let dir = TestDir::new();
        let mut cfg = config();
        cfg.logging.log_dir = Some(dir.0.join("absent"));
        let ctx = AnyCtx::new(cfg);
        assert!(get_json_logs(&ctx).await.is_empty());
        assert!(!dir.0.join("absent").exists());
    }

    #[tokio::test]
    async fn client_startup_reports_invalid_logging_configuration() {
        let mut cfg = config();
        cfg.logging.max_file = 0;
        let err = crate::Client::start(cfg)
            .wait_until_dead()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("logging.max_file"));
    }

    #[test]
    fn invalid_limits_and_unwritable_destinations_report_errors() {
        let dir = TestDir::new();
        let mut config = LoggingConfig::default();
        config.max_file = 0;
        assert!(rotating_file(&dir.log(), &config).is_err());
        config.max_file = 1;
        config.max_file_size = 0;
        assert!(rotating_file(&dir.log(), &config).is_err());
        config.max_file_size = 1024;
        config.level = "verbose".into();
        assert!(rotating_file(&dir.log(), &config).is_err());
        config.level = "info".into();
        assert!(rotating_file(&dir.0, &config).is_err());
    }

    #[test]
    fn default_paths_distinguish_cache_filenames_and_accounts() {
        let mut cfg = config();
        cfg.cache = Some("client.db".into());
        assert_eq!(log_path(&cfg), PathBuf::from("./client.db.log"));
        cfg.cache = Some("client.sqlite".into());
        assert_eq!(log_path(&cfg), PathBuf::from("./client.sqlite.log"));
        cfg.cache = None;
        let first = log_path(&cfg);
        cfg.credentials = geph5_broker_protocol::Credential::Secret("different-account".into());
        assert_ne!(first, log_path(&cfg));
    }

    #[tokio::test]
    async fn legacy_migration_preserves_credentials_and_reclaims_pages() {
        let dir = TestDir::new();
        let db_path = dir.0.join("cache.db");
        let mut cfg = config();
        cfg.cache = Some(db_path.clone());
        let ctx = AnyCtx::new(cfg);
        crate::database::db_write(&ctx, "auth_token", b"keep-me")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE logs (json BLOB)")
            .execute(ctx.get(DATABASE))
            .await
            .unwrap();
        sqlx::query("INSERT INTO logs VALUES (zeroblob(1048576))")
            .execute(ctx.get(DATABASE))
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(ctx.get(DATABASE))
            .await
            .unwrap();
        let file_size_before = fs::metadata(&db_path).unwrap().len();
        let before: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(ctx.get(DATABASE))
            .await
            .unwrap();
        migrate_legacy_logs(&ctx).await.unwrap();
        let after: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(ctx.get(DATABASE))
            .await
            .unwrap();
        assert!(after < before);
        assert!(fs::metadata(&db_path).unwrap().len() < file_size_before);
        assert_eq!(
            crate::database::db_read(&ctx, "auth_token").await.unwrap(),
            Some(b"keep-me".to_vec())
        );
        let logs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name = 'logs'")
                .fetch_one(ctx.get(DATABASE))
                .await
                .unwrap();
        assert_eq!(logs, 0);
        migrate_legacy_logs(&ctx).await.unwrap();
        ctx.get(DATABASE).close().await;
    }

    #[tokio::test]
    async fn interrupted_legacy_compaction_is_retried() {
        let ctx = AnyCtx::new(config());
        crate::database::db_write(&ctx, "legacy_logs_vacuum_pending", b"1")
            .await
            .unwrap();
        migrate_legacy_logs(&ctx).await.unwrap();
        assert!(
            crate::database::db_read(&ctx, "legacy_logs_vacuum_pending")
                .await
                .unwrap()
                .is_none()
        );
    }
}
