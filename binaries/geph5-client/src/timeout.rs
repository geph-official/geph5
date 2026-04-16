use std::time::Duration;

use async_trait::async_trait;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use smol_timeout2::TimeoutExt as _;

pub const BROKER_RPC_TIMEOUT: Duration = Duration::from_secs(15);

pub struct TimeoutTransport<T> {
    inner: T,
    timeout: Duration,
}

impl<T> TimeoutTransport<T> {
    pub fn new(inner: T, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

pub trait RpcTransportExt: RpcTransport + Sized {
    fn timeout(self, timeout: Duration) -> TimeoutTransport<Self> {
        TimeoutTransport::new(self, timeout)
    }
}

impl<T: RpcTransport + Sized> RpcTransportExt for T {}

#[async_trait]
impl<T> RpcTransport for TimeoutTransport<T>
where
    T: RpcTransport + Send + Sync,
    T::Error: std::fmt::Display,
{
    type Error = anyhow::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let method = req.method.clone();
        self.inner
            .call_raw(req)
            .timeout(self.timeout)
            .await
            .ok_or_else(|| {
                anyhow::anyhow!("broker RPC timed out after {:?}: {}", self.timeout, method)
            })?
            .map_err(|err| anyhow::anyhow!(err.to_string()))
    }
}
