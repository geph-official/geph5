use anyctx::AnyCtx;
use futures_concurrency::future::TryJoin;
use futures_util::AsyncReadExt;
use nursery_macro::nursery;
use sillad::listener::Listener;
use smol::future::FutureExt;

use crate::{litecopy::litecopy, session::open_conn, Config};

pub async fn port_forward(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let tasks = ctx
        .init()
        .port_forward
        .iter()
        .map(|fwd| async {
            let mut listener = sillad::tcp::TcpListener::bind(fwd.listen).await?;
            nursery!({
                loop {
                    let listened = listener.accept().await?;
                    let connect = fwd.connect.clone();
                    let ctx = &ctx;
                    tracing::debug!(
                        listen = display(&fwd.listen),
                        connect = display(&fwd.connect),
                        "accepted port forward"
                    );
                    let fwd = fwd.clone();
                    spawn!(async move {
                        let connected = open_conn(ctx, "tcp", &connect).await?;
                        tracing::debug!(
                            listen = display(&fwd.listen),
                            connect = display(&fwd.connect),
                            "started port forward"
                        );
                        let (read_listened, write_listened) = listened.split();
                        let (read_connected, write_connected) = connected.split();
                        litecopy(read_listened, write_connected)
                            .race(litecopy(read_connected, write_listened))
                            .await?;
                        anyhow::Ok(())
                    })
                    .detach();
                }
            })
        })
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        smol::future::pending().await
    } else {
        tasks.try_join().await.map(|_: Vec<()>| ())
    }
}
