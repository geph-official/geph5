use std::{
    convert::Infallible,
    sync::{atomic::AtomicUsize, LazyLock},
    time::{Duration, SystemTime},
};

use anyctx::AnyCtx;
use arrayref::array_ref;
use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime};
use geph5_broker_protocol::{puzzle::solve_puzzle, AccountLevel, NetStatus, VoucherInfo};

use geph5_misc_rpc::client_control::{
    ConnInfo, ConnectedInfo, ControlProtocol, ControlService, ControlUserInfo, NewsItem,
    RegistrationProgress,
};
use nanorpc::{JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use slab::Slab;

use crate::{
    broker::get_net_status, broker_client, client::CtxField, logging::get_json_logs,
    stats::stat_get_num, traffcount::TRAFF_COUNT, updates::get_update_manifest, Config,
};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserInfo {
    pub user_id: u64,
    pub level: AccountLevel,

    pub recurring: bool,
    pub expiry: Option<u64>,
}

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
        let res = broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .opaque_abtest(key, id)
            .await
            .map_err(|e| format!("{:?}", e))?;
        Ok(res)
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
        get_json_logs().split("\n").map(|s| s.to_string()).collect()
    }

    async fn check_secret(&self, secret: String) -> Result<bool, String> {
        let res = broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .get_user_info_by_cred(geph5_broker_protocol::Credential::Secret(secret))
            .await
            .map_err(|e| format!("{:?}", e))?
            .map_err(|e| format!("{:?}", e))?;
        Ok(res.is_some())
    }

    async fn user_info(&self, secret: String) -> Result<ControlUserInfo, String> {
        let res = broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .get_user_info_by_cred(geph5_broker_protocol::Credential::Secret(secret))
            .await
            .map_err(|e| format!("{:?}", e))?
            .map_err(|e| format!("{:?}", e))?
            .ok_or_else(|| "no such user".to_string())?;
        Ok(ControlUserInfo {
            user_id: res.user_id,
            level: if res.plus_expires_unix.is_some() {
                AccountLevel::Plus
            } else {
                AccountLevel::Free
            },
            expiry: res.plus_expires_unix,
            recurring: res.recurring,

            bw_consumption: res.bw_consumption,
        })
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

    async fn delete_account(&self, secret: String) -> Result<(), String> {
        tracing::debug!("FROM delete_account: secret={secret}");
        broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .delete_account(secret)
            .await
            .map_err(|e| format!("BROKER TRANSPORT ERROR: {:?}", e))?
            .map_err(|e| format!("ERROR FROM BROKER {:?}", e))?;
        Ok(())
    }

    async fn poll_registration(&self, idx: usize) -> Result<RegistrationProgress, String> {
        tracing::debug!(idx, "polling registration");
        let registers = REGISTRATIONS.lock();
        registers
            .get(idx)
            .cloned()
            .ok_or_else(|| "no such registration".to_string())
    }

    async fn convert_legacy_account(
        &self,
        username: String,
        password: String,
    ) -> Result<String, String> {
        Ok(broker_client(&self.ctx)
            .map_err(|e| format!("{:?}", e))?
            .upgrade_to_secret(geph5_broker_protocol::Credential::LegacyUsernamePassword {
                username,
                password,
            })
            .await
            .map_err(|e| format!("{:?}", e))?
            .map_err(|e| format!("{:?}", e))?)
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

    async fn get_free_voucher(&self, secret: String) -> Result<Option<VoucherInfo>, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client
            .get_free_voucher(secret)
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?)
    }

    async fn redeem_voucher(&self, secret: String, code: String) -> Result<i32, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;

        // Call the broker's redeem_voucher method directly with the secret
        client
            .redeem_voucher(secret, code)
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())
    }

    async fn price_points(&self) -> Result<Vec<(u32, f64)>, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client
            .raw_price_points()
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?
            .into_iter()
            .map(|(a, b)| (a, b as f64 / 100.0))
            .collect())
    }

    async fn basic_price_points(&self) -> Result<Vec<(u32, f64)>, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client
            .basic_price_points()
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?
            .into_iter()
            .map(|(a, b)| (a, b as f64 / 100.0))
            .collect())
    }

    async fn basic_mb_limit(&self) -> Result<u32, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client.basic_mb_limit().await.map_err(|s| s.to_string())?)
    }

    async fn payment_methods(&self) -> Result<Vec<String>, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client
            .payment_methods()
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?)
    }

    async fn create_payment(
        &self,
        secret: String,
        days: u32,
        method: String,
    ) -> Result<String, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client
            .create_payment(secret, days, method)
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?)
    }

    async fn create_basic_payment(
        &self,
        secret: String,
        days: u32,
        method: String,
    ) -> Result<String, String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        Ok(client
            .create_basic_payment(secret, days, method)
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?)
    }

    async fn export_debug_pack(
        &self,
        email: Option<String>,
        contents: String,
    ) -> Result<(), String> {
        let client = broker_client(&self.ctx).map_err(|e| format!("{:?}", e))?;
        client
            .upload_debug_pack(email, contents)
            .await
            .map_err(|s| s.to_string())?
            .map_err(|s| s.to_string())?;
        Ok(())
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
