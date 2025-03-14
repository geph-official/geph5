use std::{ops::Deref, str::FromStr, sync::LazyLock, time::Duration};

use async_io::Timer;
use geph5_broker_protocol::BridgeDescriptor;
use moka::future::Cache;

use rand::Rng;
use sqlx::{
    pool::PoolOptions,
    postgres::{PgConnectOptions, PgSslMode},
    prelude::FromRow,
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
            .execute(POSTGRES.deref())
            .await?;
        tracing::debug!(rows_affected = res.rows_affected(), "cleaned up exits");
        let res = sqlx::query("delete from bridges_new where expiry < extract(epoch from now())")
            .execute(POSTGRES.deref())
            .await?;
        tracing::debug!(rows_affected = res.rows_affected(), "cleaned up bridges");
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
        VALUES ($1, $2, $3, $4, $5, $6, extract(epoch from now()) + ($7 - extract(epoch from now())))
        ON CONFLICT (pubkey) DO UPDATE 
        SET c2e_listen = EXCLUDED.c2e_listen, 
            b2e_listen = EXCLUDED.b2e_listen, 
            country = EXCLUDED.country, 
            city = EXCLUDED.city, 
            load = EXCLUDED.load, 
            expiry = extract(epoch from now()) + (EXCLUDED.expiry - extract(epoch from now()))
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

pub async fn query_bridges(key: &str) -> anyhow::Result<Vec<(BridgeDescriptor, u32, bool)>> {
    static CACHE: LazyLock<Cache<String, Vec<(BridgeDescriptor, u32, bool)>>> =
        LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(300))
                .build()
        });

    // // shuffle
    // let key = format!(
    //     "{key}-{}",
    //     SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 3600
    // );

    CACHE
        .try_get_with(key.to_string(), async {
            let raw: Vec<(String, String, String, i64, i32, bool)> = sqlx::query_as(
                r"
WITH selected_bridges AS (
    SELECT DISTINCT ON (bn.pool)
        bn.listen,
        bn.cookie,
        bn.pool,
        bn.expiry,
        COALESCE(bgd.delay_ms, 0)     AS delay,
        COALESCE(bgd.is_plus, false)  AS is_plus
    FROM bridges_new bn
    LEFT JOIN bridge_group_delays bgd 
           ON bn.pool = bgd.pool
    ORDER BY 
        bn.pool,
        ENCODE(DIGEST(bn.listen || $1, 'sha256'), 'hex')
),
updated AS (
    UPDATE bridges_new bn2
       SET alloc_count = alloc_count + 1
      FROM selected_bridges sb
     WHERE bn2.listen = sb.listen
    RETURNING bn2.listen, bn2.alloc_count
)
SELECT 
    sb.listen,
    sb.cookie,
    sb.pool,
    sb.expiry,
    sb.delay,
    sb.is_plus
FROM selected_bridges sb
JOIN updated u ON u.listen = sb.listen;

        ",
            )
            .bind(key)
            .fetch_all(POSTGRES.deref())
            .await?;
            anyhow::Ok(
                raw.into_iter()
                    .map(|row| {
                        (
                            BridgeDescriptor {
                                control_listen: row.0.parse().unwrap(),
                                control_cookie: row.1,
                                pool: row.2,
                                expiry: row.3 as _,
                            },
                            row.4 as _,
                            row.5,
                        )
                    })
                    .collect(),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
}
