use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, LazyLock,
    },
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use dashmap::DashMap;
use deadpool::managed::{Metrics, Pool, RecycleResult};
use futures_util::{AsyncRead, AsyncReadExt as _};
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlProtocol, BridgeControlService};
use moka::future::Cache;
use once_cell::sync::Lazy;
use picomux::PicoMux;
use sillad::{
    dialer::Dialer,
    listener::Listener,
    tcp::{TcpDialer, TcpListener},
};
use smol::future::FutureExt as _;
use stdcode::StdcodeSerializeExt;
use tap::Tap;

pub async fn listen_forward_loop(my_ip: IpAddr, listener: impl Listener) -> anyhow::Result<()> {
    let state = State { my_ip };
    nanorpc_sillad::rpc_serve(listener, BridgeControlService(state)).await?;
    Ok(())
}

#[allow(clippy::type_complexity)]
struct State {
    // b2e_dest => (metadata, task)
    my_ip: IpAddr,
}

#[async_trait]
impl BridgeControlProtocol for State {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr {
        static MAPPING: LazyLock<
            Cache<(SocketAddr, B2eMetadata), (SocketAddr, Arc<smol::Task<anyhow::Result<()>>>)>,
        > = LazyLock::new(|| {
            Cache::builder()
                .time_to_idle(Duration::from_secs(3600))
                .build()
        });

        MAPPING
            .get_with((b2e_dest, metadata.clone()), async {
                let listener = TcpListener::bind("0.0.0.0:0".parse().unwrap())
                    .await
                    .unwrap();
                let addr = listener
                    .local_addr()
                    .await
                    .tap_mut(|s| s.set_ip(self.my_ip));
                let task = smolscale::spawn(handle_one_listener(listener, b2e_dest, metadata));
                (addr, Arc::new(task))
            })
            .await
            .0
    }
}

async fn handle_one_listener(
    mut listener: impl Listener,
    b2e_dest: SocketAddr,
    metadata: B2eMetadata,
) -> anyhow::Result<()> {
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    loop {
        let client_conn = listener.accept().await?;
        let count = COUNT.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(count, b2e_dest = debug(b2e_dest), "handled a connection");
        let metadata = metadata.clone();
        smolscale::spawn(async move {
            scopeguard::defer!({
                COUNT.fetch_sub(1, Ordering::Relaxed);
            });
            let exit_conn = dial_pooled(b2e_dest, &metadata.stdcode())
                .await
                .inspect_err(|e| tracing::warn!("cannot dial pooled: {:?}", e))?;
            let (client_read, client_write) = client_conn.split();
            let (exit_read, exit_write) = exit_conn.split();
            smol::io::copy(ByteCounter(exit_read), client_write)
                .race(smol::io::copy(ByteCounter(client_read), exit_write))
                .await?;
            anyhow::Ok(())
        })
        .detach();
    }
}

pub static BYTE_COUNT: AtomicU64 = AtomicU64::new(0);

struct ByteCounter<R>(R);

impl<R: AsyncRead + Unpin> AsyncRead for ByteCounter<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = unsafe { Pin::map_unchecked_mut(self, |this| &mut this.0) }.poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &poll {
            BYTE_COUNT.fetch_add(*n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        poll
    }
}

async fn dial_pooled(b2e_dest: SocketAddr, metadata: &[u8]) -> anyhow::Result<picomux::Stream> {
    static POOLS: Lazy<DashMap<u8, Cache<SocketAddr, Pool<MuxManager>>>> =
        Lazy::new(Default::default);
    let pool = POOLS
        .entry(rand::random::<u8>() % 10)
        .or_insert_with(|| {
            Cache::builder()
                .time_to_idle(Duration::from_secs(600))
                .build()
        })
        .try_get_with(b2e_dest, async {
            Pool::builder(MuxManager {
                underlying: TcpDialer {
                    dest_addr: b2e_dest,
                },
            })
            .max_size(20)
            .build()
        })
        .await
        .context(format!("cannot get pool, b2e_dest={b2e_dest}"))?;
    let mux = pool
        .get()
        .await
        .context(format!("cannot get from pool, b2e_dest={b2e_dest}"))?;
    let stream = mux
        .open(metadata)
        .await
        .context(format!("cannot open through mux, b2e_dest={b2e_dest}"))?;
    Ok(stream)
}

struct MuxManager {
    underlying: TcpDialer,
}

impl deadpool::managed::Manager for MuxManager {
    type Type = picomux::PicoMux;
    type Error = std::io::Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let dialed = self.underlying.dial().await?;
        let (read, write) = dialed.split();
        let mux = PicoMux::new(read, write);
        Ok(mux)
    }
    async fn recycle(&self, conn: &mut Self::Type, _: &Metrics) -> RecycleResult<Self::Error> {
        tracing::debug!(alive = conn.is_alive(), "trying to recycle");
        if !conn.is_alive() {
            let error = conn.open(b"").await.expect_err("dude");
            return Err(error.into());
        }
        Ok(())
    }
}
