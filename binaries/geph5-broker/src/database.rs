use std::{ops::Deref, str::FromStr, time::Duration};

use async_io::Timer;
use once_cell::sync::Lazy;
use rand::Rng;
use sqlx::{
    pool::PoolOptions,
    postgres::{PgConnectOptions, PgSslMode},
    prelude::FromRow,
    PgPool,
};

use crate::CONFIG_FILE;

pub static POSTGRES: Lazy<PgPool> = Lazy::new(|| {
    smolscale::block_on(
        PoolOptions::new()
            .max_connections(160)
            .acquire_timeout(Duration::from_secs(10))
            .max_lifetime(Duration::from_secs(600))
            .connect_with(
                PgConnectOptions::from_str(&CONFIG_FILE.wait().postgres_url)
                    .unwrap()
                    .ssl_mode(PgSslMode::VerifyFull)
                    .ssl_root_cert(&CONFIG_FILE.wait().postgres_root_cert),
            ),
    )
    .unwrap()
});

/// This loop is used for garbage-collecting stale data from the database.
#[tracing::instrument]
pub async fn database_gc_loop() -> anyhow::Result<()> {
    tracing::info!("starting the database GC loop");
    loop {
        let sleep_time = Duration::from_secs_f64(rand::thread_rng().gen_range(60.0..120.0));
        tracing::debug!("sleeping {:?}", sleep_time);
        Timer::after(sleep_time).await;
        let res = sqlx::query("delete from exits_new where expiry > extract(epoch from now())")
            .execute(POSTGRES.deref())
            .await?;
        tracing::debug!(rows_affected = res.rows_affected(), "cleaned up exits");
    }
}

#[derive(FromRow)]
pub struct ExitRow {
    pub pubkey: [u8; 32],
    pub c2e_listen: String,
    pub b2e_listen: String,
    pub country: String,
    pub city: String,
    pub load: f32,
    pub expiry: i64,
}

pub async fn insert_exit(exit: &ExitRow) -> anyhow::Result<()> {
    sqlx::query(
        r"INSERT INTO exits_new (pubkey, c2e_listen, b2e_listen, country, city, load, expiry)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (pubkey) DO UPDATE 
        SET c2e_listen = EXCLUDED.c2e_listen, 
            b2e_listen = EXCLUDED.b2e_listen, 
            country = EXCLUDED.country, 
            city = EXCLUDED.city, 
            load = EXCLUDED.load, 
            expiry = EXCLUDED.expiry
        ",
    )
    .bind(exit.pubkey)
    .bind(&exit.c2e_listen)
    .bind(&exit.b2e_listen)
    .bind(&exit.country)
    .bind(&exit.city)
    .bind(exit.load)
    .bind(exit.expiry)
    .execute(POSTGRES.deref())
    .await?;
    Ok(())
}
