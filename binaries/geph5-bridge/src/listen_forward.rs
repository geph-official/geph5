use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use deadpool::managed::Pool;
use futures_util::AsyncReadExt as _;
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

pub async fn listen_forward_loop(my_ip: IpAddr, listener: impl Listener) {
    let state = State {
        my_ip,
        mapping: Cache::builder()
            .time_to_idle(Duration::from_secs(86400))
            .build(),
    };
    nanorpc_sillad::rpc_serve(listener, BridgeControlService(state))
        .await
        .unwrap()
}

struct State {
    // b2e_dest => (metadata, task)
    my_ip: IpAddr,
    mapping: Cache<(SocketAddr, B2eMetadata), (SocketAddr, Arc<smol::Task<anyhow::Result<()>>>)>,
}

#[async_trait]
impl BridgeControlProtocol for State {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr {
        self.mapping
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
    loop {
        let client_conn = listener.accept().await?;
        let metadata = metadata.clone();
        smolscale::spawn(async move {
            let exit_conn = dial_pooled(b2e_dest, &metadata.stdcode())
                .await
                .inspect_err(|e| tracing::warn!("cannot dial pooled: {:?}", e))?;
            let (client_read, client_write) = client_conn.split();
            let (exit_read, exit_write) = exit_conn.split();
            smol::io::copy(exit_read, client_write)
                .race(smol::io::copy(client_read, exit_write))
                .await?;
            anyhow::Ok(())
        })
        .detach();
    }
}

async fn dial_pooled(b2e_dest: SocketAddr, metadata: &[u8]) -> anyhow::Result<picomux::Stream> {
    static POOLS: Lazy<Cache<SocketAddr, Pool<MuxManager>>> = Lazy::new(|| {
        Cache::builder()
            .time_to_idle(Duration::from_secs(86400))
            .build()
    });
    let pool = POOLS
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
        .context("cannot get pool")?;
    let mux = pool.get().await.context("cannot get from pool")?;
    let stream = mux
        .open(metadata)
        .await
        .context("cannot open through mux")?;
    Ok(stream)
}

struct MuxManager {
    underlying: TcpDialer,
}

#[async_trait]
impl deadpool::managed::Manager for MuxManager {
    type Type = picomux::PicoMux;
    type Error = std::io::Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let dialed = self.underlying.dial().await?;
        let (read, write) = dialed.split();
        let mux = PicoMux::new(read, write);
        Ok(mux)
    }
    async fn recycle(
        &self,
        conn: &mut Self::Type,
        _: &deadpool::managed::Metrics,
    ) -> deadpool::managed::RecycleResult<Self::Error> {
        tracing::debug!(alive = conn.is_alive(), "trying to recycle");
        if !conn.is_alive() {
            let error = conn.open(b"").await.expect_err("dude");
            return Err(error.into());
        }
        Ok(())
    }
}
