use std::{str::FromStr, sync::LazyLock, time::Duration};

use async_io::Timer;
use rand::Rng;
use sqlx::{
    PgPool,
    pool::PoolOptions,
    postgres::{PgConnectOptions, PgSslMode},
};

use crate::CONFIG_FILE;

static POSTGRES: LazyLock<PgPool> = LazyLock::new(|| {
    smolscale::block_on(
        PoolOptions::new()
            .max_connections(500)
            .acquire_timeout(Duration::from_secs(10))
            .max_lifetime(Duration::from_secs(30))
            .test_before_acquire(false)
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
        if rand::random::<f64>() < 0.001 {
            sqlx::query("vacuum full exits_new")
                .execute(&*POSTGRES)
                .await?;
            sqlx::query("vacuum full bridges_new")
                .execute(&*POSTGRES)
                .await?;
        }
    }
}

pub mod auth;
pub mod bandwidth;

pub mod bridges;
pub mod exits;
pub mod free_voucher;
pub mod puzzle;
pub mod self_stat;
