use anyctx::AnyCtx;
use futures_concurrency::future::Race;
use futures_concurrency::future::TryJoin;
use sillad::listener::Listener;

use crate::{Config, litecopy::litecopy, session::open_conn};

pub async fn port_forward(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    // Owns the per-connection forwarders so they are cancelled when this future
    // is dropped (replaces the old `nursery!` scoped executor).
    let reaper: geph5_rt::TaskReaper<anyhow::Result<()>> = geph5_rt::TaskReaper::new();
    let reaper = &reaper;
    let tasks = ctx
        .init()
        .port_forward
        .iter()
        .map(|fwd| async move {
            let mut listener = sillad::tcp::TcpListener::bind(fwd.listen).await?;
            loop {
                let listened = listener.accept().await?;
                let connect = fwd.connect.clone();
                let ctx = ctx.clone();
                tracing::debug!(
                    listen = display(&fwd.listen),
                    connect = display(&fwd.connect),
                    "accepted port forward"
                );
                let fwd = fwd.clone();
                reaper.attach(geph5_rt::spawn(async move {
                    let connected = open_conn(&ctx, "tcp", &connect).await?;
                    tracing::debug!(
                        listen = display(&fwd.listen),
                        connect = display(&fwd.connect),
                        "started port forward"
                    );
                    let (read_listened, write_listened) = tokio::io::split(listened);
                    let (read_connected, write_connected) = tokio::io::split(connected);
                    (
                        litecopy(read_listened, write_connected),
                        litecopy(read_connected, write_listened),
                    )
                        .race()
                        .await?;
                    anyhow::Ok(())
                }));
            }
            #[allow(unreachable_code)]
            anyhow::Ok::<()>(())
        })
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        std::future::pending().await
    } else {
        tasks.try_join().await.map(|_: Vec<()>| ())
    }
}
