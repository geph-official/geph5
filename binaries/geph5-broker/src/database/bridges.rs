use super::POSTGRES;

use anyhow::Context;
use geph5_broker_protocol::BridgeDescriptor;
use moka::future::Cache;
use sha2::{Digest, Sha256};
use smol::lock::Semaphore;
use smol_timeout2::TimeoutExt;
use std::{
    collections::HashMap,
    collections::hash_map::Entry,
    sync::LazyLock,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct BridgeMetadata {
    pub descriptor: BridgeDescriptor,
    pub delay_ms: u32,
    pub is_plus: bool,
    pub china_success_count: u32,
    pub china_fail_count: u32,
}

pub async fn query_bridges(key: &str) -> anyhow::Result<Vec<BridgeMetadata>> {
    let key = u64::from_le_bytes(
        blake3::hash(key.as_bytes()).as_bytes()[..8]
            .try_into()
            .unwrap(),
    );

    let bridges = cached_bridges().await?;
    Ok(rendezvous_bridges(bridges, key))
}

fn rendezvous_bridges(bridges: Vec<BridgeMetadata>, key: u64) -> Vec<BridgeMetadata> {
    let mut chosen: HashMap<String, (u128, BridgeMetadata)> = HashMap::new();

    for bridge in bridges {
        let pool = bridge.descriptor.pool.clone();
        let listen = bridge.descriptor.control_listen.to_string();
        let score = rendezvous_score(key, &listen);
        match chosen.entry(pool) {
            Entry::Vacant(entry) => {
                entry.insert((score, bridge));
            }
            Entry::Occupied(mut entry) => {
                if score < entry.get().0 {
                    entry.insert((score, bridge));
                }
            }
        }
    }

    chosen.into_values().map(|(_, meta)| meta).collect()
}

fn rendezvous_score(key: u64, listen: &str) -> u128 {
    let mut hasher = Sha256::new();
    hasher.update(listen.as_bytes());
    hasher.update(key.to_le_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    u128::from_be_bytes(digest[..16].try_into().unwrap())
}

async fn cached_bridges() -> anyhow::Result<Vec<BridgeMetadata>> {
    static CACHE: LazyLock<Cache<(), Vec<BridgeMetadata>>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(30))
            .build()
    });

    CACHE
        .try_get_with((), async {
            let start = Instant::now();
            let raw: Vec<(String, String, String, i64, i32, bool, i64, i64)> = sqlx::query_as(
                r#"WITH recent_probes AS (
    SELECT
        ip_addr,
        SUM((is_blocked = false)::INT) AS success_count,
        SUM((is_blocked = true)::INT)  AS fail_count
    FROM censorship_probes
    WHERE authority = 'gfw'
      AND time > NOW() - INTERVAL '5 minutes'
    GROUP BY ip_addr
)
SELECT
    bn.listen,
    bn.cookie,
    bn.pool,
    bn.expiry,
    COALESCE(bgd.delay_ms, 0)     AS delay,
    COALESCE(bgd.is_plus, false)  AS is_plus,
    COALESCE(rp.success_count, 0),
    COALESCE(rp.fail_count, 0)
FROM bridges_new bn
LEFT JOIN bridge_group_delays bgd
       ON bn.pool = bgd.pool
LEFT JOIN recent_probes rp ON rp.ip_addr = split_part(bn.listen, ':', 1)"#,
            )
            .fetch_all(&*POSTGRES)
            .await?;
            tracing::debug!(elapsed = debug(start.elapsed()), "fetched bridges from DB");
            anyhow::Ok(
                raw.into_iter()
                    .map(|row| BridgeMetadata {
                        descriptor: geph5_broker_protocol::BridgeDescriptor {
                            control_listen: row.0.parse().unwrap(),
                            control_cookie: row.1,
                            pool: row.2,
                            expiry: row.3 as _,
                        },
                        delay_ms: row.4 as _,
                        is_plus: row.5,
                        china_success_count: row.6 as _,
                        china_fail_count: row.7 as _,
                    })
                    .collect(),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

pub async fn insert_bridge(descriptor: &BridgeDescriptor) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO bridges_new (listen, cookie, pool, expiry)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (listen) DO UPDATE
            SET cookie = $2, pool = $3, expiry = $4"#,
    )
    .bind(descriptor.control_listen.to_string())
    .bind(descriptor.control_cookie.to_string())
    .bind(descriptor.pool.to_string())
    .bind(descriptor.expiry as i64)
    .execute(&*POSTGRES)
    .await?;
    Ok(())
}

use geph5_broker_protocol::AvailabilityData;
use std::time::SystemTime;

pub async fn record_availability(data: AvailabilityData) -> anyhow::Result<()> {
    let current_timestamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut txn = POSTGRES.begin().await?;
    let up_time: Option<(i64,)> = sqlx::query_as(
        "select last_update from bridge_availability where listen = $1 and user_country = $2 and user_asn = $3",
    )
    .bind(&data.listen)
    .bind(&data.country)
    .bind(&data.asn)
    .fetch_optional(&mut *txn)
    .await?;
    if let Some((up_time,)) = up_time {
        let diff = current_timestamp.saturating_sub(up_time) as f64;
        let decay_factor = 2.0f64.powf(diff / 3600.0);
        if data.success {
            sqlx::query("update bridge_availability set successes = successes / $1 + 1, last_update = $2 where listen = $3 and user_country = $4 and user_asn = $5")
                .bind(decay_factor)
                .bind(current_timestamp)
                .bind(&data.listen)
                .bind(&data.country)
                .bind(&data.asn)
                .execute(&mut *txn)
                .await?;
        } else {
            sqlx::query("update bridge_availability set failures = failures / $1 + 1, last_update = $2 where listen = $3 and user_country = $4 and user_asn = $5")
                .bind(decay_factor)
                .bind(current_timestamp)
                .bind(&data.listen)
                .bind(&data.country)
                .bind(&data.asn)
                .execute(&mut *txn)
                .await?;
        }
    } else if data.success {
        sqlx::query("insert into bridge_availability (listen, user_country, user_asn, successes, failures, last_update) values ($1, $2, $3, 1.0, 0.0, $4)")
            .bind(&data.listen)
            .bind(&data.country)
            .bind(&data.asn)
            .bind(current_timestamp)
            .execute(&mut *txn)
            .await?;
    } else {
        sqlx::query("insert into bridge_availability (listen, user_country, user_asn, successes, failures, last_update) values ($1, $2, $3, 0.0, 1.0, $4)")
            .bind(&data.listen)
            .bind(&data.country)
            .bind(&data.asn)
            .bind(current_timestamp)
            .execute(&mut *txn)
            .await?;
    }
    txn.commit().await?;
    Ok(())
}
