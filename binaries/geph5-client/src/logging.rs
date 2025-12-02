use anyctx::AnyCtx;
use async_channel::{Receiver, Sender};
use chrono::Utc;
use std::io::{self, Write};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::{client::Config, database::DATABASE};

struct DbLogWriter {
    tx: Sender<Vec<u8>>,
}

impl Write for DbLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Non-blocking: if the channel is full or closed, drop the log
        let _ = self.tx.try_send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
} 

async fn ensure_logs_table(ctx: &AnyCtx<Config>) -> Result<(), sqlx::Error> {
    // Ensure table exists
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            json TEXT NOT NULL
        );",
    )
    .execute(ctx.get(DATABASE))
    .await?;

    // Ensure an index on timestamp for faster range deletes/queries
    sqlx::query("CREATE INDEX IF NOT EXISTS logs_ts_idx ON logs(ts);")
        .execute(ctx.get(DATABASE))
        .await?;

    // Delete logs older than 24 hours (run once per startup)
    let cutoff = Utc::now().timestamp() - 24 * 60 * 60; // seconds
    sqlx::query("DELETE FROM logs WHERE ts < ?")
        .bind(cutoff)
        .execute(ctx.get(DATABASE))
        .await?;

    Ok(())
}

fn spawn_log_consumer(ctx: AnyCtx<Config>, rx: Receiver<Vec<u8>>) {
    smolscale::spawn(async move {
        if ensure_logs_table(&ctx).await.is_err() {
            // If we can't ensure the table, abort consumer silently
            return;
        }

        // Accumulate partial lines between channel messages
        let mut carry: Vec<u8> = Vec::new();
        while let Ok(mut chunk) = rx.recv().await {
            if !carry.is_empty() {
                carry.extend_from_slice(&chunk);
                chunk = std::mem::take(&mut carry);
            }

            let mut start = 0usize;
            for i in 0..chunk.len() {
                if chunk[i] == b'\n' {
                    let line = &chunk[start..i];
                    if !line.is_empty() {
                        let _ = sqlx::query("INSERT INTO logs (ts, json) VALUES (?, ?)")
                            .bind(Utc::now().timestamp())
                            .bind(String::from_utf8_lossy(line).to_string())
                            .execute(ctx.get(DATABASE))
                            .await;
                    }
                    start = i + 1;
                }
            }
            if start < chunk.len() {
                carry.extend_from_slice(&chunk[start..]);
            }
        }
    })
    .detach();
}

/// Initialize the tracing subscribers for logging, storing JSON logs in SQLite
pub fn init_logging(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    // Channel and consumer that writes logs into DB
    let (tx, rx) = async_channel::unbounded::<Vec<u8>>();
    spawn_log_consumer(ctx.clone(), rx);

    #[cfg(not(target_os = "ios"))]
    tracing_subscriber::registry()
        // Standard logs to stderr (for console display)
        .with(fmt::layer().compact().with_writer(std::io::stderr))
        // JSON logs persisted via DB writer
        .with(
            fmt::layer()
                .json()
                .with_writer(move || DbLogWriter { tx: tx.clone() }),
        )
        // Set filtering based on environment or defaults
        .with(
            EnvFilter::builder()
                .with_default_directive("geph=debug".parse()?)
                .from_env_lossy(),
        )
        .try_init()?;

    #[cfg(target_os = "ios")]
    tracing_subscriber::registry()
        // Standard logs to stderr (for console display)
        .with(OsLogger::new("geph.io.daemon", "default"))
        // JSON logs persisted via DB writer
        .with(
            fmt::layer()
                .json()
                .with_writer(move || DbLogWriter { tx: tx.clone() }),
        )
        // Set filtering based on environment or defaults
        .with(
            EnvFilter::builder()
                .with_default_directive("geph=debug".parse()?)
                .from_env_lossy(),
        )
        .try_init()?;

    Ok(())
}

/// Get the current JSON logs as a String by reading from DB
pub async fn get_json_logs(ctx: &AnyCtx<Config>) -> String {
    match sqlx::query_scalar::<_, String>("SELECT json FROM logs ORDER BY id ASC")
        .fetch_all(ctx.get(DATABASE))
        .await
    {
        Ok(lines) => lines.join("\n"),
        Err(_) => String::new(),
    }
}
