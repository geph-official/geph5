use std::{
    collections::BTreeMap,
    convert::Infallible,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyctx::AnyCtx;
use async_trait::async_trait;
use geph5_broker_protocol::ExitDescriptor;

use nanorpc::{nanorpc_derive, JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{client::CtxField, logs::LOGS, stats::stat_get_num, Config};

#[nanorpc_derive]
#[async_trait]
pub trait ControlProtocol {
    async fn conn_info(&self) -> ConnInfo;
    async fn stat_num(&self, stat: String) -> f64;
    async fn start_time(&self) -> SystemTime;
    async fn stop(&self);

    async fn recent_logs(&self) -> Vec<(SystemTime, String, BTreeMap<SmolStr, String>)>;
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ConnInfo {
    Connecting,
    Connected(ConnectedInfo),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConnectedInfo {
    pub protocol: String,
    pub bridge: String,

    pub exit: ExitDescriptor,
}

pub struct ControlProtocolImpl {
    pub ctx: AnyCtx<Config>,
}

pub static CURRENT_CONN_INFO: CtxField<Mutex<ConnInfo>> = |_| Mutex::new(ConnInfo::Connecting);

#[async_trait]
impl ControlProtocol for ControlProtocolImpl {
    async fn conn_info(&self) -> ConnInfo {
        self.ctx.get(CURRENT_CONN_INFO).lock().clone()
    }

    async fn stat_num(&self, stat: String) -> f64 {
        stat_get_num(&self.ctx, &stat)
    }

    async fn start_time(&self) -> SystemTime {
        static START_TIME: CtxField<SystemTime> = |_| SystemTime::now();
        *self.ctx.get(START_TIME)
    }

    async fn stop(&self) {
        smolscale::spawn(async move {
            smol::Timer::after(Duration::from_millis(100)).await;
            std::process::exit(0);
        })
        .detach();
    }

    async fn recent_logs(&self) -> Vec<(SystemTime, String, BTreeMap<SmolStr, String>)> {
        let logs = LOGS.read();
        logs.iter()
            .map(|log| {
                let msg = log.fields.get("message").map(|s| s.as_str()).unwrap_or("");
                (
                    chrono_to_system_time(log.timestamp),
                    format!("{} {}", log.level, msg),
                    log.fields.clone(),
                )
            })
            .collect()
    }
}

fn chrono_to_system_time(dt: chrono::DateTime<chrono::Utc>) -> SystemTime {
    let duration_since_epoch = dt.timestamp_nanos_opt().unwrap();
    let std_duration = Duration::from_nanos(duration_since_epoch as u64);
    UNIX_EPOCH + std_duration
}

pub struct DummyControlProtocolTransport(pub ControlService<ControlProtocolImpl>);

#[async_trait]
impl RpcTransport for DummyControlProtocolTransport {
    type Error = Infallible;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        Ok(self.0.respond_raw(req).await)
    }
}
