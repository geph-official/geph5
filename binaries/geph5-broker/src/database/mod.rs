use std::{ops::Deref, str::FromStr, sync::LazyLock, time::Duration};

use anyhow::Context;
use async_io::Timer;
use geph5_broker_protocol::ExitMetadata;
use rand::Rng;
use sqlx::{
    pool::PoolOptions,
    postgres::{PgConnectOptions, PgSslMode},
    PgPool,
};

use crate::CONFIG_FILE;

pub static POSTGRES: LazyLock<PgPool> = LazyLock::new(|| {
    smolscale::block_on(
        PoolOptions::new()
            .max_connections(300)
            .acquire_timeout(Duration::from_secs(60))
            .max_lifetime(Duration::from_secs(600))
            .connect_with({
                let cfg = CONFIG_FILE.wait();
                let mut opts = PgConnectOptions::from_str(&cfg.postgres_url).unwrap();
                if let Some(postgres_root_cert) = &cfg.postgres_root_cert {
                    opts = opts
                        .ssl_mode(PgSslMode::VerifyFull)
                        .ssl_root_cert(postgres_root_cert);
                }
                opts
            }),
    )
    .unwrap()
});

/// This loop is used for garbage-collecting stale data from the database.
#[tracing::instrument]
pub async fn database_gc_loop() -> anyhow::Result<()> {
    tracing::info!("starting the database GC loop");
    loop {
        let sleep_time = Duration::from_secs_f64(rand::thread_rng().gen_range(1.0..2.0));
        tracing::debug!("sleeping {:?}", sleep_time);
        Timer::after(sleep_time).await;
        let res = sqlx::query("delete from exits_new where expiry < extract(epoch from now())")
            .execute(&*POSTGRES)
            .await?;
        tracing::debug!(rows_affected = res.rows_affected(), "cleaned up exits");
        let res = sqlx::query("delete from bridges_new where expiry < extract(epoch from now())")
            .execute(&*POSTGRES)
            .await?;
        tracing::debug!(rows_affected = res.rows_affected(), "cleaned up bridges");
    }
}

pub mod exits;
pub mod bridges;
pub mod bandwidth;
pub mod auth;
pub mod puzzle;
pub mod free_voucher;
pub mod self_stat;

/// Initialize the database connection pool
pub fn init() {
    LazyLock::force(&POSTGRES);
}

