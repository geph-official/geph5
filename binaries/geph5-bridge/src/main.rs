mod asn_count;
mod listen_forward;

use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    thread::available_parallelism,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use asn_count::ASN_BYTES;
use geph5_broker_protocol::{BridgeDescriptor, Mac};
use listen_forward::{BYTE_COUNT, listen_forward_loop};
use rand::Rng;
use sillad::{
    dialer::DialerExt,
    tcp::{TcpDialer, TcpListener},
};
use sillad_sosistab3::{Cookie, listener::SosistabListener};
use smol::future::FutureExt as _;

use smol_timeout2::TimeoutExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    if std::env::var("GEPH5_BRIDGE_POOL")
        .unwrap()
        .contains("yaofan")
    {
        smolscale::permanently_single_threaded();
        if std::env::var("GEPH5_BRIDGE_CHILD").is_err() {
            for _ in 0..available_parallelism().unwrap().get() {
                std::thread::spawn(|| {
                    unsafe {
                        std::env::set_var("GEPH5_BRIDGE_CHILD", "1");
                    }
                    let current_exe = std::env::current_exe().unwrap();

                    // Collect the current command-line arguments
                    let args: Vec<String> = std::env::args().collect();

                    // Trim the first argument which is the current binary's path
                    let args_to_pass = &args[1..];

                    // Spawn a new process with the same command
                    let mut child = std::process::Command::new(current_exe)
                        .args(args_to_pass)
                        .spawn()
                        .unwrap();

                    // Wait for the spawned process to finish
                    child.wait().unwrap();
                });
            }
            loop {
                std::thread::park();
            }
        }
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_bridge".parse().unwrap())
                .from_env_lossy(),
        )
        .init();
    smolscale::block_on(async {
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

        let port = rand::rng().random_range(1024..10000);
        let control_listen = SocketAddr::new(my_ip, port);
        let control_cookie = format!("bridge-cookie-{}", rand::random::<u128>());

        let upload_loop = broker_loop(control_listen, control_cookie.clone());
        let listen_loop = async {
            loop {
                let listener = TcpListener::bind(format!("0.0.0.0:{port}").parse().unwrap())
                    .await
                    .unwrap();

                let control_listener =
                    SosistabListener::new(listener, Cookie::new(&control_cookie));
                if let Err(err) = listen_forward_loop(my_ip, control_listener).await {
                    tracing::error!(err = %err, "error in listen_forward_loop");
                }
                smol::Timer::after(Duration::from_secs(1)).await;
            }
        };
        upload_loop.race(listen_loop).await
    })
}

async fn broker_loop(control_listen: SocketAddr, control_cookie: String) {
    let auth_token = std::env::var("GEPH5_BRIDGE_TOKEN").unwrap();
    let pool = std::env::var("GEPH5_BRIDGE_POOL").unwrap();
    let broker_addr: SocketAddr = std::env::var("GEPH5_BROKER_ADDR").unwrap().parse().unwrap();
    tracing::info!(
        auth_token,
        broker_addr = display(broker_addr),
        "starting upload loop"
    );

    let bridge_key = format!("bridges.{pool}");

    let broker_rpc = Arc::new(geph5_broker_protocol::BrokerClient(
        nanorpc_sillad::DialerTransport(
            TcpDialer {
                dest_addr: broker_addr,
            }
            .timeout(Duration::from_secs(1)),
        ),
    ));

    let upload_loop = async {
        loop {
            tracing::info!(
                auth_token,
                broker_addr = display(broker_addr),
                "uploading..."
            );

            let res = async {
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
                    .timeout(Duration::from_secs(2))
                    .await
                    .context("insert bridge timed out")??
                    .map_err(|e| anyhow::anyhow!(e))?;
                anyhow::Ok(())
            };
            if let Err(err) = res.await {
                tracing::error!(err = %err, "error in upload_loop");
            }
            smol::Timer::after(Duration::from_secs(10)).await;
        }
    };

    let stats_loop = async {
        loop {
            tracing::info!(auth_token, broker_addr = display(broker_addr), "stats...");
            let res = async {
                let byte_count = BYTE_COUNT.swap(0, std::sync::atomic::Ordering::Relaxed);
                broker_rpc
                    .incr_stat(format!("{bridge_key}.byte_count"), byte_count as _)
                    .timeout(Duration::from_secs(2))
                    .await
                    .context("incrementing bytes timed out")??;

                let asn_bytes: Vec<(u32, u64)> = ASN_BYTES
                    .iter()
                    .map(|item| {
                        let asn_byte_count =
                            item.value().swap(0, std::sync::atomic::Ordering::Relaxed);
                        (*item.key(), asn_byte_count)
                    })
                    .collect();
                ASN_BYTES.clear();
                for (asn, bytes) in asn_bytes {
                    let bytes = bytes.min(i32::MAX as u64) as i32;
                    let broker_rpc = broker_rpc.clone();
                    let bridge_key = bridge_key.clone();
                    smolscale::spawn(async move {
                        broker_rpc
                            .incr_stat(format!("{bridge_key}.asn.{}", asn), bytes)
                            .timeout(Duration::from_secs(2))
                            .await
                            .context("incrementing ASN timed out")??;
                        tracing::debug!("incremented ASN {} with {} bytes", asn, bytes);
                        anyhow::Ok(())
                    })
                    .detach();
                }
                anyhow::Ok(())
            };
            if let Err(err) = res.await {
                tracing::error!(err = %err, "error in stats_loop");
            }
            smol::Timer::after(Duration::from_secs(3)).await;
        }
    };
    upload_loop.race(stats_loop).await
}
