use anyctx::AnyCtx;
use event_listener::Event;
use sqlx::{pool::PoolOptions, Row};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::str::FromStr;

use crate::client::{Config, CtxField};

static DATABASE: CtxField<SqlitePool> = |ctx| {
    // TODO this somehow does not make all the connections share the same db?
    let db_path = ctx
        .init()
        .cache
        .as_ref()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ":memory:".into());
    tracing::debug!("INITIALIZING DATABASE");
    let options = dbg!(SqliteConnectOptions::from_str(&db_path))
        .unwrap()
        .create_if_missing(true);

    smol::future::block_on(async move {
        let pool = PoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .max_lifetime(None)
            .idle_timeout(None)
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
