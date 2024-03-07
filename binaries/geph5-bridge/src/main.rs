use std::{
    alloc::System,
    net::SocketAddr,
    time::{Duration, SystemTime},
};

use geph5_broker_protocol::{BridgeDescriptor, Mac};
use sillad::tcp::{TcpDialer, TcpListener};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_bridge".parse().unwrap())
                .from_env_lossy(),
        )
        .init();
    smolscale::block_on(async {
        let listener = TcpListener::bind("0.0.0.0:0".parse().unwrap())
            .await
            .unwrap();
        todo!()
    })
}

async fn broker_upload_loop(control_listen: SocketAddr, control_cookie: String) {
    let auth_token = std::env::var("GEPH5_BRIDGE_TOKEN").unwrap();
    let broker_addr: SocketAddr = std::env::var("GEPH5_BROKER_ADDR").unwrap().parse().unwrap();
    tracing::info!(
        auth_token,
        broker_addr = display(broker_addr),
        "starting upload loop"
    );

    let broker_rpc =
        geph5_broker_protocol::BrokerClient(nanorpc_sillad::DialerTransport(TcpDialer {
            dest_addr: broker_addr,
        }));
    loop {
        broker_rpc
            .put_bridge(Mac::new(
                BridgeDescriptor {
                    control_listen,
                    control_cookie: control_cookie.clone(),
                    expiry: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 600,
                },
                blake3::hash(auth_token.as_bytes()).as_bytes(),
            ))
            .await
            .unwrap()
            .unwrap();
        async_io::Timer::after(Duration::from_secs(60)).await;
    }
}
