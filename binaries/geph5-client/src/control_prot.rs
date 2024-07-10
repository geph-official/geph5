use std::{convert::Infallible, time::SystemTime};

use anyctx::AnyCtx;
use async_trait::async_trait;
use isocountry::CountryCode;
use nanorpc::{nanorpc_derive, JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use serde::{Deserialize, Serialize};

use crate::{client::CtxField, stats::stat_get_num, Config};

#[nanorpc_derive]
#[async_trait]
pub trait ControlProtocol {
    async fn conn_info(&self) -> ConnInfo;
    async fn stat_num(&self, stat: String) -> f64;
    async fn start_time(&self) -> SystemTime;
}

#[derive(Serialize, Deserialize)]
pub enum ConnInfo {
    Connecting,
    Connected(ConnectedInfo),
}

#[derive(Serialize, Deserialize)]
pub struct ConnectedInfo {
    pub protocol: String,
    pub bridge: String,

    pub exit: String,
    pub country: CountryCode,
    pub city: String,
}

pub(crate) struct ControlProtocolImpl {
    pub ctx: AnyCtx<Config>,
}

#[async_trait]
impl ControlProtocol for ControlProtocolImpl {
    async fn conn_info(&self) -> ConnInfo {
        todo!()
    }

    async fn stat_num(&self, stat: String) -> f64 {
        stat_get_num(&self.ctx, &stat)
    }

    async fn start_time(&self) -> SystemTime {
        static START_TIME: CtxField<SystemTime> = |_| SystemTime::now();
        *self.ctx.get(START_TIME)
    }
}

pub(crate) struct DummyControlProtocolTransport(pub ControlService<ControlProtocolImpl>);

#[async_trait]
impl RpcTransport for DummyControlProtocolTransport {
    type Error = Infallible;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        Ok(self.0.respond_raw(req).await)
    }
}
