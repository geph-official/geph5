use std::{str::FromStr, sync::LazyLock, time::Duration};

use sqlx::{
    Executor, PgPool,
    pool::PoolOptions,
    postgres::{PgConnectOptions, PgSslMode},
};

use crate::CONFIG_FILE;

pub(super) static POSTGRES: LazyLock<PgPool> = LazyLock::new(|| {
    PoolOptions::new()
        .max_connections(150)
        // Acquisition also covers opening a connection, which can take several
        // seconds after database recovery, including during broker startup.
        .acquire_timeout(Duration::from_secs(10))
        .max_lifetime(Duration::from_secs(30))
        .test_before_acquire(false)
        .connect_lazy_with({
            let cfg = CONFIG_FILE.wait();
            let mut opts = PgConnectOptions::from_str(&cfg.postgres_url).unwrap();
            if let Some(postgres_root_cert) = &cfg.postgres_root_cert {
                opts = opts
                    .ssl_mode(PgSslMode::VerifyFull)
                    .ssl_root_cert(postgres_root_cert);
            }
            opts
        })
});

/// Install shared maintenance jobs once at startup. The transaction lock also
/// serializes brokers starting together, including through the pooler.
pub async fn install_maintenance_jobs() -> anyhow::Result<()> {
    let mut txn = POSTGRES.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('geph_broker_maintenance_setup'))")
        .execute(&mut *txn)
        .await?;
    (&mut *txn)
        .execute(include_str!("../../sql/login_counts.sql"))
        .await?;
    (&mut *txn)
        .execute(include_str!("../../sql/routing_cleanup.sql"))
        .await?;
    txn.commit().await?;
    Ok(())
}

pub mod auth;
pub mod bandwidth;

pub mod bridges;
pub mod exits;
pub mod free_voucher;
pub mod puzzle;
pub mod self_stat;
