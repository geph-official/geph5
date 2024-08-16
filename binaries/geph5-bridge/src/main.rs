mod listen_forward;

use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    time::{Duration, SystemTime},
};

use geph5_broker_protocol::{BridgeDescriptor, Mac};
use listen_forward::{listen_forward_loop, BYTE_COUNT};
use sillad::tcp::{TcpDialer, TcpListener};
use sillad_sosistab3::{listener::SosistabListener, Cookie};
use smol::future::FutureExt as _;
use tap::Tap;
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
        let my_ip = IpAddr::from_str(
            String::from_utf8_lossy(
                &reqwest::get("https://checkip.amazonaws.com/")
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap(),
            )
            .trim(),
        )
        .unwrap();
        let control_listen = listener
            .local_addr()
            .await
            .tap_mut(|addr| addr.set_ip(my_ip));
        let control_cookie = format!("bridge-cookie-{}", rand::random::<u128>());
        let control_listener = SosistabListener::new(listener, Cookie::new(&control_cookie));
        let upload_loop = broker_upload_loop(control_listen, control_cookie);
        let listen_loop = listen_forward_loop(my_ip, control_listener);
        upload_loop.race(listen_loop).await
    })
}

async fn broker_upload_loop(control_listen: SocketAddr, control_cookie: String) {
    let auth_token = std::env::var("GEPH5_BRIDGE_TOKEN").unwrap();
    let pool = std::env::var("GEPH5_BRIDGE_POOL").unwrap();
    let broker_addr: SocketAddr = std::env::var("GEPH5_BROKER_ADDR").unwrap().parse().unwrap();
    tracing::info!(
        auth_token,
        broker_addr = display(broker_addr),
        "starting upload loop"
    );

    let bridge_key = format!("bridges.{pool}");

    let broker_rpc =
        geph5_broker_protocol::BrokerClient(nanorpc_sillad::DialerTransport(TcpDialer {
            dest_addr: broker_addr,
        }));

    loop {
        tracing::info!(
            auth_token,
            broker_addr = display(broker_addr),
            "uploading..."
        );
        let byte_count = BYTE_COUNT.swap(0, std::sync::atomic::Ordering::Relaxed);
        broker_rpc
            .incr_stat(format!("{bridge_key}.byte_count"), byte_count as _)
            .await
            .unwrap();
        broker_rpc
            .insert_bridge(Mac::new(
                BridgeDescriptor {
                    control_listen,
                    control_cookie: control_cookie.clone(),
                    pool: pool.clone(),
                    expiry: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 120,
                },
                blake3::hash(auth_token.as_bytes()).as_bytes(),
            ))
            .await
            .unwrap()
            .unwrap();
        smol::Timer::after(Duration::from_secs(10)).await;
    }
}
