use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use geph5_broker_protocol::{
    AccountLevel, BrokerClient, DOMAIN_EXIT_DESCRIPTOR, ExitDescriptor, JsonSigned, Mac, StatEvent,
};
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Method;
use tap::Tap;

use crate::{
    AdvertisedIpSource, CONFIG_FILE, SIGNING_SECRET, exit_metadata,
    ratelimit::{get_kbps, get_load},
    schedlag::SCHEDULER_LAG_SECS,
    tasklimit::get_task_count,
};

pub static ACCEPT_FREE: AtomicBool = AtomicBool::new(false);
const ADVERTISED_IP_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const ADVERTISED_IP_STALE_AFTER: Duration = Duration::from_secs(120);

pub struct BrokerRpcTransport {
    url: String,
    client: reqwest::Client,
}

impl BrokerRpcTransport {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: reqwest::ClientBuilder::new()
                .timeout(Duration::from_secs(10))
                .http1_only()
                .build()
                .unwrap(),
        }
    }
}

#[async_trait]
impl RpcTransport for BrokerRpcTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::trace!(
            method = req.method,
            params = display(serde_json::to_string(&req.params).unwrap()),
            "calling broker"
        );

        let resp = self
            .client
            .request(Method::POST, &self.url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&req).unwrap())
            .send()
            .await
            .inspect_err(|e| tracing::warn!(err = debug(e), "contacting broker failed"))?;
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AdvertisedIpLookupSource {
    Static(IpAddr),
    Hostname(String),
    CheckIp,
}

impl From<Option<AdvertisedIpSource>> for AdvertisedIpLookupSource {
    fn from(source: Option<AdvertisedIpSource>) -> Self {
        match source {
            Some(AdvertisedIpSource::Static(ip)) => Self::Static(ip),
            Some(AdvertisedIpSource::Hostname(hostname)) => Self::Hostname(hostname),
            None => Self::CheckIp,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CachedAdvertisedIp {
    ip: IpAddr,
    resolved_at: Instant,
}

#[derive(Clone, Debug)]
struct AdvertisedIpCache {
    source: AdvertisedIpLookupSource,
    cached: Option<CachedAdvertisedIp>,
}

impl AdvertisedIpCache {
    fn new(source: AdvertisedIpLookupSource) -> Self {
        Self {
            source,
            cached: None,
        }
    }

    async fn current_ip(&mut self) -> anyhow::Result<IpAddr> {
        match &self.source {
            AdvertisedIpLookupSource::Static(ip) => Ok(*ip),
            source => {
                let now = Instant::now();
                if let Some(cached) = self.fresh_cached_ip(now) {
                    return Ok(cached);
                }

                let resolved = resolve_advertised_ip(source).await;
                self.apply_resolution(now, resolved)
            }
        }
    }

    fn fresh_cached_ip(&self, now: Instant) -> Option<IpAddr> {
        self.cached
            .filter(|cached| {
                now.duration_since(cached.resolved_at) < ADVERTISED_IP_REFRESH_INTERVAL
            })
            .map(|cached| cached.ip)
    }

    fn apply_resolution(
        &mut self,
        now: Instant,
        resolved: anyhow::Result<IpAddr>,
    ) -> anyhow::Result<IpAddr> {
        match resolved {
            Ok(ip) => {
                if self.cached.is_none_or(|cached| cached.ip != ip) {
                    tracing::info!(ip = display(ip), "advertised exit IP changed");
                }
                self.cached = Some(CachedAdvertisedIp {
                    ip,
                    resolved_at: now,
                });
                Ok(ip)
            }
            Err(err) => {
                if let Some(cached) = self.cached.filter(|cached| {
                    now.duration_since(cached.resolved_at) < ADVERTISED_IP_STALE_AFTER
                }) {
                    tracing::warn!(
                        err = debug(&err),
                        ip = display(cached.ip),
                        age_secs = now.duration_since(cached.resolved_at).as_secs(),
                        "reusing last advertised exit IP after refresh failed"
                    );
                    Ok(cached.ip)
                } else {
                    Err(err
                        .context("advertised exit IP is unavailable and no recent value is cached"))
                }
            }
        }
    }
}

async fn resolve_advertised_ip(source: &AdvertisedIpLookupSource) -> anyhow::Result<IpAddr> {
    match source {
        AdvertisedIpLookupSource::Static(ip) => Ok(*ip),
        AdvertisedIpLookupSource::Hostname(hostname) => resolve_hostname_ip(hostname).await,
        AdvertisedIpLookupSource::CheckIp => resolve_check_ip().await,
    }
}

async fn resolve_hostname_ip(hostname: &str) -> anyhow::Result<IpAddr> {
    let lookup = format!("{hostname}:0");
    let addrs = tokio::net::lookup_host(&lookup)
        .await
        .with_context(|| format!("could not resolve advertised exit hostname {hostname}"))?;
    addrs
        .into_iter()
        .map(|addr| addr.ip())
        .next()
        .with_context(|| format!("advertised exit hostname {hostname} resolved to no addresses"))
}

async fn resolve_check_ip() -> anyhow::Result<IpAddr> {
    static CHECK_IP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
    });
    let body = CHECK_IP_CLIENT
        .get("https://checkip.amazonaws.com/")
        .send()
        .await?
        .bytes()
        .await?;
    IpAddr::from_str(String::from_utf8_lossy(&body).trim())
        .context("checkip response was not an IP")
}

fn descriptor_for_ip(my_ip: IpAddr, load: f32) -> ExitDescriptor {
    ExitDescriptor {
        c2e_listen: listen_addr_for_ip(CONFIG_FILE.wait().c2e_listen, my_ip),
        b2e_listen: listen_addr_for_ip(CONFIG_FILE.wait().b2e_listen, my_ip),
        country: CONFIG_FILE.wait().country,
        city: CONFIG_FILE.wait().city.clone(),
        load,
        expiry: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 120,
    }
}

fn listen_addr_for_ip(listen: SocketAddr, my_ip: IpAddr) -> SocketAddr {
    listen.tap_mut(|addr| addr.set_ip(my_ip))
}

#[tracing::instrument]
pub async fn broker_loop() -> anyhow::Result<()> {
    match &CONFIG_FILE.wait().broker {
        Some(broker) => {
            let transport = BrokerRpcTransport::new(&broker.url);
            let client = BrokerClient(transport);
            let mut advertised_ip =
                AdvertisedIpCache::new(CONFIG_FILE.wait().ip_addr.clone().into());
            let my_pubkey: VerifyingKey = (&*SIGNING_SECRET).into();

            loop {
                let upload = async {
                    let my_ip = advertised_ip.current_ip().await?;
                    tracing::info!(
                        c2e_direct = format!(
                            "{}:{}/{}",
                            my_ip,
                            CONFIG_FILE.wait().c2e_listen.port(),
                            hex::encode(my_pubkey.as_bytes())
                        ),
                        "listen information gotten"
                    );
                    let server_name = format!(
                        "{}-{}",
                        CONFIG_FILE.wait().country.alpha2().to_lowercase(),
                        my_ip.to_string().replace('.', "-")
                    );

                    let free_exits = client
                        .get_free_exits()
                        .await?
                        .map_err(|e| anyhow::anyhow!(e))?
                        .inner
                        .all_exits;
                    if CONFIG_FILE.wait().free_ratelimit > 0 {
                        let accept_free = free_exits
                            .iter()
                            .map(|s| s.0)
                            .any(|vk| vk == (&*SIGNING_SECRET).into());
                        tracing::info!("accept free? {:?}", accept_free);
                        ACCEPT_FREE.store(accept_free, Ordering::Relaxed);
                    }

                    static START_TIME: LazyLock<Instant> = LazyLock::new(Instant::now);
                    let load = get_load();
                    let exit_tag: &[(&str, &str)] = &[("exit", &server_name)];
                    let mut stats = vec![
                        StatEvent::gauge("kbps", exit_tag, get_kbps() as _),
                        StatEvent::gauge("uptime", exit_tag, START_TIME.elapsed().as_secs_f64()),
                        StatEvent::gauge("load", exit_tag, load as _),
                        StatEvent::gauge("task_count", exit_tag, get_task_count() as _),
                        StatEvent::gauge(
                            "schedlag",
                            exit_tag,
                            SCHEDULER_LAG_SECS.load(Ordering::Relaxed),
                        ),
                    ];
                    // "free" = free-eligible (allowed_levels), not the broker's live
                    // free rotation, so exits don't hop between dashboard groups.
                    let level = if exit_metadata().allowed_levels.contains(&AccountLevel::Free) {
                        "free"
                    } else {
                        "plus"
                    };
                    stats.extend(crate::google_selfcheck::google_selfcheck_stat_events(
                        &server_name,
                        level,
                    ));
                    client
                        .report_stats(Mac::new(
                            stats,
                            blake3::hash(broker.auth_token.as_bytes()).as_bytes(),
                        ))
                        .await?
                        .map_err(|e| anyhow::anyhow!(e.0))?;

                    let descriptor = descriptor_for_ip(my_ip, load);
                    let metadata = exit_metadata();
                    let to_upload = Mac::new(
                        JsonSigned::new(
                            (descriptor, metadata),
                            DOMAIN_EXIT_DESCRIPTOR,
                            &SIGNING_SECRET,
                        ),
                        blake3::hash(broker.auth_token.as_bytes()).as_bytes(),
                    );
                    client
                        .insert_exit_v2(to_upload)
                        .await?
                        .map_err(|e| anyhow::anyhow!(e.0))?;
                    anyhow::Ok(())
                };
                if let Err(err) = upload.await {
                    tracing::warn!(err = debug(err), "failed to upload descriptor")
                }
                tokio::time::sleep(Duration::from_millis(2000)).await;
            }
        }
        None => {
            tracing::info!("not starting broker loop since there's no binder URL");
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ADVERTISED_IP_REFRESH_INTERVAL, ADVERTISED_IP_STALE_AFTER, AdvertisedIpCache,
        AdvertisedIpLookupSource, CachedAdvertisedIp, listen_addr_for_ip,
    };
    use std::{
        net::IpAddr,
        time::{Duration, Instant},
    };

    #[test]
    fn advertised_ip_refresh_updates_cached_value() {
        let mut cache = AdvertisedIpCache::new(AdvertisedIpLookupSource::Hostname(
            "exit.example.org".into(),
        ));
        let now = Instant::now();

        assert_eq!(
            cache
                .apply_resolution(now, Ok("203.0.113.1".parse().unwrap()))
                .unwrap(),
            "203.0.113.1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            cache
                .apply_resolution(
                    now + ADVERTISED_IP_REFRESH_INTERVAL + Duration::from_secs(1),
                    Ok("203.0.113.2".parse().unwrap())
                )
                .unwrap(),
            "203.0.113.2".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            cache.cached.unwrap().ip,
            "203.0.113.2".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn fresh_cached_ip_prevents_unnecessary_refresh() {
        let now = Instant::now();
        let cache = AdvertisedIpCache {
            source: AdvertisedIpLookupSource::CheckIp,
            cached: Some(CachedAdvertisedIp {
                ip: "203.0.113.1".parse().unwrap(),
                resolved_at: now,
            }),
        };

        assert_eq!(
            cache.fresh_cached_ip(now + ADVERTISED_IP_REFRESH_INTERVAL - Duration::from_secs(1)),
            Some("203.0.113.1".parse().unwrap())
        );
        assert_eq!(
            cache.fresh_cached_ip(now + ADVERTISED_IP_REFRESH_INTERVAL),
            None
        );
    }

    #[test]
    fn failed_refresh_reuses_only_recent_cached_ip() {
        let now = Instant::now();
        let mut cache = AdvertisedIpCache {
            source: AdvertisedIpLookupSource::CheckIp,
            cached: Some(CachedAdvertisedIp {
                ip: "203.0.113.1".parse().unwrap(),
                resolved_at: now,
            }),
        };

        assert_eq!(
            cache
                .apply_resolution(
                    now + ADVERTISED_IP_STALE_AFTER - Duration::from_secs(1),
                    Err(anyhow::anyhow!("dns failed"))
                )
                .unwrap(),
            "203.0.113.1".parse::<IpAddr>().unwrap()
        );
        assert!(
            cache
                .apply_resolution(
                    now + ADVERTISED_IP_STALE_AFTER,
                    Err(anyhow::anyhow!("dns failed"))
                )
                .is_err()
        );
    }

    #[test]
    fn advertised_ip_replaces_bind_ip_but_preserves_port() {
        assert_eq!(
            listen_addr_for_ip(
                "0.0.0.0:9002".parse().unwrap(),
                "203.0.113.5".parse().unwrap()
            ),
            "203.0.113.5:9002".parse().unwrap()
        );
    }
}
