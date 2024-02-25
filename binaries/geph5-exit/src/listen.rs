use std::time::{Duration, SystemTime};

use futures_util::{AsyncReadExt, TryFutureExt};
use geph5_broker_protocol::{BrokerClient, ExitDescriptor, Mac, Signed, DOMAIN_EXIT_DESCRIPTOR};
use picomux::PicoMux;
use sillad::{listener::Listener, tcp::TcpListener, Pipe};
use smol::future::FutureExt as _;

use crate::{broker::BrokerRpcTransport, proxy::proxy_stream, CONFIG_FILE, SIGNING_SECRET};

pub async fn listen_main() -> anyhow::Result<()> {
    let c2e = c2e_loop();
    let broker = broker_loop();
    c2e.race(broker).await
}

async fn broker_loop() -> anyhow::Result<()> {
    match CONFIG_FILE.wait().broker_url.as_ref() {
        Some(binder) => {
            let transport = BrokerRpcTransport::new(binder);
            let client = BrokerClient(transport);
            loop {
                let descriptor = ExitDescriptor {
                    c2e_listen: CONFIG_FILE.wait().c2e_listen,
                    b2e_listen: CONFIG_FILE.wait().b2e_listen,
                    country: CONFIG_FILE.wait().country,
                    city: CONFIG_FILE.wait().city.clone(),
                    load: 0.0,
                    expiry: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 600,
                };
                let to_upload = Mac::new(
                    Signed::new(descriptor, DOMAIN_EXIT_DESCRIPTOR, &SIGNING_SECRET),
                    blake3::hash(CONFIG_FILE.wait().broker_auth_token.as_bytes()).as_bytes(),
                );
                client
                    .put_exit(to_upload)
                    .await?
                    .map_err(|e| anyhow::anyhow!(e.0))?;

                smol::Timer::after(Duration::from_secs(60)).await;
            }
        }
        None => {
            tracing::info!("not starting broker loop since there's no binder URL");
            smol::future::pending().await
        }
    }
}

async fn c2e_loop() -> anyhow::Result<()> {
    let mut listener = TcpListener::bind(CONFIG_FILE.wait().c2e_listen).await?;
    loop {
        let c2e_raw = listener.accept().await?;
        smolscale::spawn(
            handle_client(c2e_raw).map_err(|e| tracing::debug!("client died suddenly with {e}")),
        )
        .detach()
    }
}

async fn handle_client(client: impl Pipe) -> anyhow::Result<()> {
    // TODO actually authenticate the client
    let (client_read, client_write) = client.split();
    let mut mux = PicoMux::new(client_read, client_write);
    loop {
        let stream = mux.accept().await?;
        smolscale::spawn(proxy_stream(stream).map_err(|e| tracing::debug!("stream died with {e}")))
            .detach();
    }
}
