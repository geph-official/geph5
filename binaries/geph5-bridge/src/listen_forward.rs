use std::{net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::AsyncReadExt as _;
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlProtocol, BridgeControlService};
use moka::future::Cache;
use sillad::{listener::Listener, tcp::TcpListener};
use smol::future::FutureExt as _;

use stdcode::StdcodeSerializeExt;

pub async fn listen_forward_loop(listener: impl Listener) {
    let state = State {
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
    mapping: Cache<SocketAddr, (SocketAddr, Arc<smol::Task<anyhow::Result<()>>>)>,
}

#[async_trait]
impl BridgeControlProtocol for State {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr {
        self.mapping
            .get_with(b2e_dest, async {
                let listener = TcpListener::bind("0.0.0.0:0".parse().unwrap())
                    .await
                    .unwrap();
                let addr = listener.local_addr().await;
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
            let exit_conn = dial_pooled(b2e_dest, &metadata.stdcode()).await?;
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

async fn dial_pooled(_b2e_dest: SocketAddr, _metadata: &[u8]) -> anyhow::Result<picomux::Stream> {
    todo!()
}
