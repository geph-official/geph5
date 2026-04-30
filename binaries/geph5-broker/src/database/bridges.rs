use super::POSTGRES;

use geph5_broker_protocol::BridgeDescriptor;
use moka::future::Cache;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
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

#[derive(Clone, Debug)]
struct BridgePoolDelay {
    pool_prefix: String,
    delay_ms: u32,
    is_plus: bool,
}

async fn ensure_bridge_pool_delays_schema() -> anyhow::Result<()> {
    sqlx::query(
        r#"do $$
        begin
            perform pg_advisory_xact_lock(hashtext('bridge_pool_delays_migration'));
            if to_regclass('bridge_group_delays') is not null
               and to_regclass('bridge_pool_delays') is null then
                alter table bridge_group_delays rename to bridge_pool_delays;
            end if;
        end
        $$;"#,
    )
    .execute(&*POSTGRES)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS bridge_pool_delays (
            pool text primary key,
            delay_ms integer not null,
            is_plus boolean not null default false
        )"#,
    )
    .execute(&*POSTGRES)
    .await?;

    sqlx::query(
        r#"do $$
        begin
            perform pg_advisory_xact_lock(hashtext('bridge_pool_delays_migration'));
            if to_regclass('bridge_group_delays') is not null then
                insert into bridge_pool_delays (pool, delay_ms, is_plus)
                select pool, delay_ms, is_plus
                from bridge_group_delays
                on conflict (pool) do update
                set delay_ms = excluded.delay_ms,
                    is_plus = excluded.is_plus;
                drop table bridge_group_delays;
            end if;
        end
        $$;"#,
    )
    .execute(&*POSTGRES)
    .await?;

    Ok(())
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

const ROUTES_PER_BRIDGE_POOL: usize = 2;

fn rendezvous_bridges(bridges: Vec<BridgeMetadata>, key: u64) -> Vec<BridgeMetadata> {
    let mut chosen: HashMap<String, Vec<(u128, BridgeMetadata)>> = HashMap::new();

    for bridge in bridges {
        let pool = bridge.descriptor.pool.clone();
        let listen = bridge.descriptor.control_listen.to_string();
        let score = rendezvous_score(key, &listen);
        let pool_routes = chosen.entry(pool).or_default();
        pool_routes.push((score, bridge));
        pool_routes.sort_unstable_by_key(|(score, _)| *score);
        if pool_routes.len() > ROUTES_PER_BRIDGE_POOL {
            pool_routes.truncate(ROUTES_PER_BRIDGE_POOL);
        }
    }

    chosen
        .into_values()
        .flatten()
        .map(|(_, meta)| meta)
        .collect()
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
            ensure_bridge_pool_delays_schema().await?;
            let start = Instant::now();
            let probes: Vec<(String, i64, i64)> = sqlx::query_as(
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
SELECT ip_addr, success_count, fail_count
FROM recent_probes"#,
            )
            .fetch_all(&*POSTGRES)
            .await?;
            let raw: Vec<(String, String, String, i64)> = sqlx::query_as(
                r#"SELECT
    bn.listen,
    bn.cookie,
    bn.pool,
    bn.expiry
FROM bridges_new bn"#,
            )
            .fetch_all(&*POSTGRES)
            .await?;
            let pool_delays: Vec<(String, i32, bool)> = sqlx::query_as(
                r#"SELECT pool, delay_ms, is_plus
FROM bridge_pool_delays"#,
            )
            .fetch_all(&*POSTGRES)
            .await?;
            tracing::debug!(elapsed = debug(start.elapsed()), "fetched bridges from DB");
            let probe_map: HashMap<_, _> = probes
                .into_iter()
                .map(|(ip, success, fail)| (ip, (success, fail)))
                .collect();
            let pool_delays: Vec<_> = pool_delays
                .into_iter()
                .map(|(pool_prefix, delay_ms, is_plus)| BridgePoolDelay {
                    pool_prefix,
                    delay_ms: delay_ms as _,
                    is_plus,
                })
                .collect();
            anyhow::Ok(
                raw.into_iter()
                    .map(|row| {
                        let control_listen: SocketAddr = row.0.parse().unwrap();
                        let (china_success_count, china_fail_count) = probe_map
                            .get(&listen_host(&control_listen))
                            .copied()
                            .unwrap_or((0, 0));
                        let matched_delay = longest_matching_pool_delay(&pool_delays, &row.2);
                        BridgeMetadata {
                            descriptor: geph5_broker_protocol::BridgeDescriptor {
                                control_listen,
                                control_cookie: row.1,
                                pool: row.2,
                                expiry: row.3 as _,
                            },
                            delay_ms: matched_delay.map(|m| m.delay_ms).unwrap_or(0),
                            is_plus: matched_delay.map(|m| m.is_plus).unwrap_or(false),
                            china_success_count: china_success_count as _,
                            china_fail_count: china_fail_count as _,
                        }
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

fn listen_host(listen: &SocketAddr) -> String {
    listen.ip().to_string()
}

fn longest_matching_pool_delay<'a>(
    pool_delays: &'a [BridgePoolDelay],
    pool: &str,
) -> Option<&'a BridgePoolDelay> {
    pool_delays
        .iter()
        .filter(|delay| pool.starts_with(&delay.pool_prefix))
        .max_by_key(|delay| delay.pool_prefix.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bridge(pool: &str, listen: &str) -> BridgeMetadata {
        BridgeMetadata {
            descriptor: BridgeDescriptor {
                control_listen: listen.parse().unwrap(),
                control_cookie: "cookie".into(),
                pool: pool.into(),
                expiry: 0,
            },
            delay_ms: 0,
            is_plus: false,
            china_success_count: 0,
            china_fail_count: 0,
        }
    }

    #[test]
    fn listen_host_handles_ipv4_and_ipv6() {
        let ipv4: SocketAddr = "1.2.3.4:9000".parse().unwrap();
        let ipv6: SocketAddr = "[2001:db8::1]:9000".parse().unwrap();
        assert_eq!(listen_host(&ipv4), "1.2.3.4");
        assert_eq!(listen_host(&ipv6), "2001:db8::1");
    }

    #[test]
    fn longest_matching_pool_delay_uses_prefix() {
        let delays = vec![BridgePoolDelay {
            pool_prefix: "ls_ap_northeast_2".into(),
            delay_ms: 123,
            is_plus: true,
        }];
        let matched = longest_matching_pool_delay(&delays, "ls_ap_northeast_2_ipv6").unwrap();
        assert_eq!(matched.delay_ms, 123);
        assert!(matched.is_plus);
    }

    #[test]
    fn longest_matching_pool_delay_prefers_more_specific_prefix() {
        let delays = vec![
            BridgePoolDelay {
                pool_prefix: "ls_ap_northeast_2".into(),
                delay_ms: 100,
                is_plus: false,
            },
            BridgePoolDelay {
                pool_prefix: "ls_ap_northeast_2_ipv6".into(),
                delay_ms: 200,
                is_plus: true,
            },
        ];
        let matched = longest_matching_pool_delay(&delays, "ls_ap_northeast_2_ipv6").unwrap();
        assert_eq!(matched.delay_ms, 200);
        assert!(matched.is_plus);
    }

    #[test]
    fn rendezvous_bridges_returns_two_from_each_pool() {
        let bridges = vec![
            test_bridge("alpha", "127.0.0.1:9001"),
            test_bridge("alpha", "127.0.0.1:9002"),
            test_bridge("alpha", "127.0.0.1:9003"),
            test_bridge("beta", "127.0.0.1:9011"),
            test_bridge("beta", "127.0.0.1:9012"),
            test_bridge("beta", "127.0.0.1:9013"),
        ];

        let selected = rendezvous_bridges(bridges, 42);
        let mut pool_counts = HashMap::new();
        for bridge in selected {
            *pool_counts.entry(bridge.descriptor.pool).or_insert(0) += 1;
        }

        assert_eq!(pool_counts.get("alpha"), Some(&ROUTES_PER_BRIDGE_POOL));
        assert_eq!(pool_counts.get("beta"), Some(&ROUTES_PER_BRIDGE_POOL));
    }
}
