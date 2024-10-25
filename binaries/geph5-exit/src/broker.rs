use std::{
    net::IpAddr,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use geph5_broker_protocol::{BrokerClient, ExitDescriptor, Mac, Signed, DOMAIN_EXIT_DESCRIPTOR};
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use reqwest::Method;
use tap::Tap;

use crate::{
    ratelimit::{get_load, TOTAL_BYTE_COUNT},
    schedlag::SCHEDULER_LAG_SECS,
    CONFIG_FILE, SIGNING_SECRET,
};

pub static ACCEPT_FREE: AtomicBool = AtomicBool::new(false);

pub struct BrokerRpcTransport {
    url: String,
    client: reqwest::Client,
}

impl BrokerRpcTransport {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl RpcTransport for BrokerRpcTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::debug!(method = req.method, "calling binder");
        let resp = self
            .client
            .request(Method::POST, &self.url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&req).unwrap())
            .send()
            .await?;
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }
}

#[tracing::instrument]
pub async fn broker_loop() -> anyhow::Result<()> {
    let my_ip = if let Some(ip_addr) = &CONFIG_FILE.wait().ip_addr {
        *ip_addr
    } else {
        IpAddr::from_str(
            String::from_utf8_lossy(
                &reqwest::get("https://checkip.amazonaws.com/")
                    .await?
                    .bytes()
                    .await?,
            )
            .trim(),
        )?
    };
    let my_pubkey: VerifyingKey = (&*SIGNING_SECRET).into();
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
    match &CONFIG_FILE.wait().broker {
        Some(broker) => {
            let transport = BrokerRpcTransport::new(&broker.url);
            let client = BrokerClient(transport);
            let mut last_byte_count = TOTAL_BYTE_COUNT.load(Ordering::Relaxed);
            loop {
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

                let upload = async {
                    let byte_count = TOTAL_BYTE_COUNT.load(Ordering::Relaxed);
                    let diff = byte_count.saturating_sub(last_byte_count);
                    last_byte_count = byte_count;
                    tracing::debug!(diff, last_byte_count, "uploaded a diff");
                    client
                        .incr_stat(format!("{server_name}.throughput"), diff as _)
                        .await?;
                    let load = get_load();
                    client
                        .set_stat(format!("{server_name}.load"), load as _)
                        .await?;
                    client
                        .set_stat(
                            format!("{server_name}.schedlag"),
                            SCHEDULER_LAG_SECS.load(Ordering::Relaxed),
                        )
                        .await?;

                    let descriptor = ExitDescriptor {
                        c2e_listen: CONFIG_FILE
                            .wait()
                            .c2e_listen
                            .tap_mut(|addr| addr.set_ip(my_ip)),
                        b2e_listen: CONFIG_FILE
                            .wait()
                            .b2e_listen
                            .tap_mut(|addr| addr.set_ip(my_ip)),
                        country: CONFIG_FILE.wait().country,
                        city: CONFIG_FILE.wait().city.clone(),
                        load,
                        expiry: SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 1800,
                    };
                    let to_upload = Mac::new(
                        Signed::new(descriptor, DOMAIN_EXIT_DESCRIPTOR, &SIGNING_SECRET),
                        blake3::hash(broker.auth_token.as_bytes()).as_bytes(),
                    );
                    client
                        .insert_exit(to_upload)
                        .await?
                        .map_err(|e| anyhow::anyhow!(e.0))?;
                    anyhow::Ok(())
                };
                if let Err(err) = upload.await {
                    tracing::warn!(err = debug(err), "failed to upload descriptor")
                }
                smol::Timer::after(Duration::from_secs_f64(fastrand::f64() * 5.0)).await;
            }
        }
        None => {
            tracing::info!("not starting broker loop since there's no binder URL");
            smol::future::pending().await
        }
    }
}
