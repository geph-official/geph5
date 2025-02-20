use std::{
    convert::Infallible,
    time::{Duration, SystemTime},
};

use anyctx::AnyCtx;
use async_trait::async_trait;
use geph5_broker_protocol::{AccountLevel, ExitDescriptor};

use itertools::Itertools;
use nanorpc::{nanorpc_derive, JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{broker_client, client::CtxField, logs::LOGS, stats::stat_get_num, Config};

#[nanorpc_derive]
#[async_trait]
pub trait ControlProtocol {
    async fn conn_info(&self) -> ConnInfo;
    async fn stat_num(&self, stat: String) -> f64;
    async fn start_time(&self) -> SystemTime;
    async fn stop(&self);

    async fn recent_logs(&self) -> Vec<String>;

    // broker-proxying stuff

    async fn check_secret(&self, secret: String) -> Result<bool, String>;
    async fn user_info(&self, secret: String) -> Result<UserInfo, String>;
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "state")]
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserInfo {
    pub level: AccountLevel,
    pub expiry: u64,
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

    async fn recent_logs(&self) -> Vec<String> {
        let logs = LOGS.lock();
        String::from_utf8_lossy(&logs)
            .split('\n')
            .map(|s| s.to_string())
            .collect_vec()
    }

    async fn check_secret(&self, secret: String) -> Result<bool, String> {
        // broker_client(&self.ctx)?.get_user_info(auth_token)
        todo!()
    }

    async fn user_info(&self, secret: String) -> Result<UserInfo, String> {
        todo!()
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
