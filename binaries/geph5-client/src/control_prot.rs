use std::{
    convert::Infallible,
    sync::{atomic::AtomicUsize, LazyLock},
    time::{Duration, SystemTime},
};

use anyctx::AnyCtx;
use arrayref::array_ref;
use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime};
use geph5_broker_protocol::{puzzle::solve_puzzle, NetStatus};

use geph5_misc_rpc::client_control::{
    ConnInfo, ConnectedInfo, ControlProtocol, ControlService, NewsItem, RegistrationProgress,
};
use nanorpc::{JrpcId, JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slab::Slab;

use crate::{
    broker::get_net_status, broker_client, client::CtxField, logging::get_json_logs,
    stats::stat_get_num, traffcount::TRAFF_COUNT, updates::get_update_manifest, Config,
};

pub struct ControlProtocolImpl {
    pub ctx: AnyCtx<Config>,
}

pub static CURRENT_CONNECTED_INFO: CtxField<Mutex<Option<ConnectedInfo>>> = |_| Mutex::new(None);
pub static CURRENT_ACTIVE_SESSIONS: CtxField<AtomicUsize> = |_| AtomicUsize::new(0);

static REGISTRATIONS: LazyLock<Mutex<Slab<RegistrationProgress>>> =
    LazyLock::new(|| Mutex::new(Slab::new()));

#[async_trait]
impl ControlProtocol for ControlProtocolImpl {
    async fn ab_test(&self, key: String, secret: String) -> Result<bool, String> {
        let id = blake3::hash(secret.as_bytes());
        let id = u64::from_be_bytes(*array_ref![id.as_bytes(), 0, 8]);
        let res = self
            .broker_rpc("opaque_abtest".into(), vec![json!(key), json!(id)])
            .await?;
        serde_json::from_value(res).map_err(|e| e.to_string())
    }

    async fn conn_info(&self) -> ConnInfo {
        if self.ctx.init().dry_run {
            return ConnInfo::Disconnected;
        }
        match self.ctx.get(CURRENT_CONNECTED_INFO).lock().clone() {
            Some(val) => {
                if self
                    .ctx
                    .get(CURRENT_ACTIVE_SESSIONS)
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > 0
                {
                    ConnInfo::Connected(val)
                } else {
                    ConnInfo::Connecting
                }
            }
            None => ConnInfo::Connecting,
        }
    }

    async fn stat_num(&self, stat: String) -> f64 {
        stat_get_num(&self.ctx, &stat)
    }

    async fn start_time(&self) -> SystemTime {
        static START_TIME: CtxField<SystemTime> = |_| SystemTime::now();
        *self.ctx.get(START_TIME)
    }

    async fn stop(&self) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            std::process::exit(0);
        });
    }

    async fn recent_logs(&self) -> Vec<String> {
        get_json_logs(&self.ctx)
            .await
            .split("\n")
            .map(|s| s.to_string())
            .collect()
    }

    async fn broker_rpc(&self, method: String, params: Vec<Value>) -> Result<Value, String> {
        let jrpc = JrpcRequest {
            jsonrpc: "2.0".into(),
            method,
            params,
            id: JrpcId::Number(1),
        };
        let resp = broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .0
            .call_raw(jrpc)
            .await
            .map_err(|e| format!("{:?}", e))?;
        if let Some(err) = resp.error {
            Err(err.message)
        } else {
            Ok(resp.result.unwrap_or(Value::Null))
        }
    }

    async fn start_registration(&self) -> Result<usize, String> {
        let (puzzle, difficulty) = broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .get_puzzle()
            .await
            .map_err(|e| format!("{:?}", e))?;
        tracing::debug!(puzzle, difficulty, "got puzzle");
        let idx = REGISTRATIONS.lock().insert(RegistrationProgress {
            progress: 0.0,
            secret: None,
        });
        let ctx = self.ctx.clone();
        smolscale::spawn(async move {
            loop {
                let fallible = async {
                    let solution = {
                        let puzzle = puzzle.clone();
                        smol::unblock(move || {
                            solve_puzzle(&puzzle, difficulty, |progress| {
                                REGISTRATIONS.lock()[idx] = RegistrationProgress {
                                    progress,
                                    secret: None,
                                }
                            })
                        })
                        .await
                    };
                    let secret = broker_client(&ctx)?
                        .register_user_secret(puzzle.clone(), solution)
                        .await?
                        .map_err(|e| anyhow::anyhow!(e))?;
                    REGISTRATIONS.lock()[idx] = RegistrationProgress {
                        progress: 1.0,
                        secret: Some(secret.clone()),
                    };
                    anyhow::Ok(secret)
                };
                if let Err(err) = fallible.await {
                    tracing::warn!(err = debug(err), "restarting registration")
                } else {
                    break;
                }
            }
        })
        .detach();
        Ok(idx)
    }

    async fn poll_registration(&self, idx: usize) -> Result<RegistrationProgress, String> {
        tracing::debug!(idx, "polling registration");
        let registers = REGISTRATIONS.lock();
        registers
            .get(idx)
            .cloned()
            .ok_or_else(|| "no such registration".to_string())
    }

    async fn stat_history(&self, stat: String) -> Result<Vec<f64>, String> {
        if stat != "traffic" {
            return Err(format!("bad: {stat}"));
        }
        Ok(self.ctx.get(TRAFF_COUNT).read().unwrap().speed_history())
    }

    async fn net_status(&self) -> Result<NetStatus, String> {
        let resp = get_net_status(&self.ctx)
            .await
            .map_err(|e| format!("{:?}", e))?;
        Ok(resp)
    }

    async fn latest_news(&self, lang: String) -> Result<Vec<NewsItem>, String> {
        let (manifest, _) = get_update_manifest().await.map_err(|e| e.to_string())?;
        let news = manifest["news"]
            .as_array()
            .ok_or_else(|| "No news array".to_string())?;

        let mut out = Vec::new();

        for item in news {
            let important = item["important"].as_bool().unwrap_or_default();
            let date_str = item["date"]
                .as_str()
                .ok_or_else(|| "No or invalid 'date' field in news item".to_string())?;

            let naive_date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .map_err(|_| format!("Invalid date format: {}", date_str))?;

            let naive_dt: NaiveDateTime = naive_date
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| "Unable to create NaiveDateTime from date".to_string())?;
            let date_unix = naive_dt.and_utc().timestamp() as u64;

            let localized = item[&lang]
                .as_object()
                .ok_or_else(|| format!("No localized data for language '{}'", lang))?;

            let title = localized["title"]
                .as_str()
                .ok_or_else(|| "Missing 'title' in localized news data".to_string())?;
            let contents = localized["contents"]
                .as_str()
                .ok_or_else(|| "Missing 'contents' in localized news data".to_string())?;

            out.push(NewsItem {
                title: title.to_string(),
                date_unix,
                contents: contents.to_string(),
                important,
            });
        }

        Ok(out)
    }

    async fn get_update_manifest(&self) -> Result<(serde_json::Value, String), String> {
        get_update_manifest().await.map_err(|e| format!("{:?}", e))
    }
}

pub struct DummyControlProtocolTransport(pub ControlService<ControlProtocolImpl>);

#[async_trait]
impl RpcTransport for DummyControlProtocolTransport {
    type Error = Infallible;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        Ok(self.0.respond_raw(req).await)
    }
}
