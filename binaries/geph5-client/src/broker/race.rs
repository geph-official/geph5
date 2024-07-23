use async_trait::async_trait;
use futures_concurrency::future::FutureGroup;
use futures_util::StreamExt;
use nanorpc::{DynRpcTransport, JrpcRequest, JrpcResponse, RpcTransport};

pub struct RaceTransport {
    choices: Vec<DynRpcTransport>,
    selected: smol::lock::Mutex<Option<usize>>,
}

impl RaceTransport {
    pub fn new(choices: Vec<DynRpcTransport>) -> Self {
        Self {
            choices,
            selected: Default::default(),
        }
    }
}

#[async_trait]
impl RpcTransport for RaceTransport {
    type Error = anyhow::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let mut selected = self.selected.lock().await;
        if let Some(idx) = selected.as_ref().copied() {
            tracing::debug!(method = &req.method, idx, "picked working transport");
            let res = self.choices[idx].call_raw(req).await;
            if res.is_err() {
                *selected = None;
            }
            res
        } else {
            tracing::debug!(
                method = &req.method,
                count = self.choices.len(),
                "racing for best transport"
            );
            let mut err = None;
            let mut future_group = FutureGroup::new();
            for (idx, choice) in self.choices.iter().enumerate() {
                let req = req.clone();
                future_group.insert(Box::pin(async move { (idx, choice.call_raw(req).await) }));
            }
            while let Some((idx, res)) = future_group.next().await {
                match res {
                    Ok(res) => {
                        *selected = Some(idx);
                        return Ok(res);
                    }
                    Err(e) => err = Some(e),
                }
            }
            Err(err.unwrap())
        }
    }
}
