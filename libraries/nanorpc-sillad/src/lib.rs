use async_executor::Executor;
use async_trait::async_trait;
use futures_util::{
    io::{AsyncWriteExt, BufReader},
    AsyncBufReadExt, AsyncReadExt,
};
use nanorpc::{JrpcRequest, JrpcResponse, RpcService, RpcTransport};
use sillad::{dialer::Dialer, listener::Listener};
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

/// Runs a given nanorpc service using the given sillad listener
pub async fn rpc_serve(
    mut listener: impl Listener,
    service: impl RpcService,
) -> std::io::Result<()> {
    let lexec = Executor::new();
    lexec
        .run(async {
            loop {
                let next = listener.accept().await?;
                lexec
                    .spawn::<anyhow::Result<()>>(async {
                        let (read, mut write) = next.split();
                        let mut read = BufReader::new(read);
                        loop {
                            let mut line = String::new();
                            read.read_line(&mut line).await?;
                            let req: JrpcRequest = serde_json::from_str(&line)?;
                            let resp = service.respond_raw(req).await;
                            write
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp)?).as_bytes(),
                                )
                                .await?;
                        }
                    })
                    .detach();
            }
        })
        .await
}
