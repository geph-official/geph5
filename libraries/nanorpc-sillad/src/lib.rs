use std::sync::Arc;

use async_trait::async_trait;
use geph5_rt::{TaskReaper, spawn};
use nanorpc::{JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use sillad::{dialer::Dialer, listener::Listener};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct DialerTransport<D: Dialer>(pub D);

#[async_trait]
impl<D: Dialer> RpcTransport for DialerTransport<D> {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        // TODO some form of connection pooling. right now there's no pooling
        let mut conn = self.0.dial().await?;
        conn.write_all(format!("{}\n", serde_json::to_string(&req)?).as_bytes())
            .await?;
        let mut conn = BufReader::new(conn);
        let mut line = String::new();
        conn.read_line(&mut line).await?;
        Ok(serde_json::from_str(&line)?)
    }
}

/// Runs a given nanorpc service using the given sillad listener.
///
/// Per-connection handlers are owned by a [`TaskReaper`], so they are all
/// cancelled when this future is dropped (structured concurrency).
pub async fn rpc_serve(
    mut listener: impl Listener,
    service: impl RpcService,
) -> std::io::Result<()> {
    let service = Arc::new(service);
    let reaper: TaskReaper<anyhow::Result<()>> = TaskReaper::new();
    loop {
        let next = listener.accept().await?;
        let service = service.clone();
        reaper.attach(spawn(async move {
            let (read, mut write) = tokio::io::split(next);
            let mut read = BufReader::new(read);
            loop {
                let mut line = String::new();
                read.read_line(&mut line).await?;
                let req: JrpcRequest = serde_json::from_str(&line)?;
                let resp = service.respond_raw(req).await;
                write
                    .write_all(format!("{}\n", serde_json::to_string(&resp)?).as_bytes())
                    .await?;
            }
        }));
    }
}
