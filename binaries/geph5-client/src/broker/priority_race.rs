use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use futures_concurrency::future::RaceOk as _;
use itertools::Itertools;
use nanorpc::{DynRpcTransport, JrpcRequest, JrpcResponse, RpcTransport};

pub struct PriorityRaceTransport {
    choices: BTreeMap<u64, DynRpcTransport>,
}

impl PriorityRaceTransport {
    pub fn new(choices: BTreeMap<u64, DynRpcTransport>) -> Self {
        Self { choices }
    }
}

#[async_trait]
impl RpcTransport for PriorityRaceTransport {
    type Error = anyhow::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let fut_vec = self
            .choices
            .iter()
            .map(|(delay, rpc)| {
                let delay = Duration::from_millis(*delay);
                let req = req.clone();
                async move {
                    smol::Timer::after(delay).await;
                    rpc.call_raw(req).await
                }
            })
            .collect_vec();
        let fastest = fut_vec.race_ok().await.map_err(|mut e| e.remove(0))?;
        Ok(fastest)
    }
}
