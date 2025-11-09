use anyctx::AnyCtx;
use event_listener::Event;
use sqlx::{pool::PoolOptions, Row};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::str::FromStr;
use stdcode::StdcodeSerializeExt;

use crate::client::{Config, CtxField};

pub static DATABASE: CtxField<SqlitePool> = |ctx| {
    let db_path = ctx
        .init()
        .cache
        .as_ref()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            dirs::config_dir()
                .map(|config_dir| {
                    config_dir
                        .join(format!(
                            "geph5-persist-{}.db",
                            hex::encode(blake3::hash(&ctx.init().credentials.stdcode()).as_bytes())
                        ))
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_else(|| {
                    log::error!(
                        "No cache path configured and dirs::config_dir() unavailable; falling back \
                         to in-memory database. Set the `cache` field in geph5-client's config to \
                         avoid hitting broker rate limits."
                    );
                    ":memory:".into()
                })
        });
    tracing::debug!("INITIALIZING DATABASE");
    let options = SqliteConnectOptions::from_str(&db_path)
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

    smol::future::block_on(async move {
        // this prevents any SQLITE_BUSY unless we have concurrent apps
        let pool = PoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .connect_lazy_with(options);

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS misc (
                key TEXT PRIMARY KEY,
                value BLOB NOT NULL
            );",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    })
};

static EVENT: CtxField<Event> = |_| Event::new();

pub async fn db_write(ctx: &AnyCtx<Config>, key: &str, value: &[u8]) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO misc (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(ctx.get(DATABASE))
        .await?;
    ctx.get(EVENT).notify(usize::MAX);
    Ok(())
}

pub async fn db_read(ctx: &AnyCtx<Config>, key: &str) -> Result<Option<Vec<u8>>, sqlx::Error> {
    let result = sqlx::query("SELECT value FROM misc WHERE key = ?")
        .bind(key)
        .fetch_optional(ctx.get(DATABASE))
        .await?
        .map(|row| row.get("value"));
    Ok(result)
}

pub async fn db_read_or_wait(ctx: &AnyCtx<Config>, key: &str) -> Result<Vec<u8>, sqlx::Error> {
    loop {
        let event = ctx.get(EVENT).listen();
        let result = db_read(ctx, key).await?;
        match result {
            Some(val) => return Ok(val),
            None => event.await,
        }
    }
}

pub async fn db_remove(ctx: &AnyCtx<Config>, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM misc WHERE key = ?")
        .bind(key)
        .execute(ctx.get(DATABASE))
        .await?;
    ctx.get(EVENT).notify(usize::MAX);
    Ok(())
}
