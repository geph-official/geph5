use anyhow::Context;
use ed25519_dalek::Signer;
use futures_util::{AsyncReadExt, TryFutureExt};
use geph5_broker_protocol::AccountLevel;
use geph5_ip_to_asn::ip_to_asn_country;
use geph5_misc_rpc::{
    bridge::B2eMetadata,
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use mizaru2::{ClientToken, UnblindedSignature};
use moka::future::Cache;
use picomux::{LivenessConfig, PicoMux};

use sillad::{listener::Listener, tcp::TcpListener, EitherPipe, Pipe};
use smol::future::FutureExt as _;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use stdcode::StdcodeSerializeExt;
use tachyonix::Sender;

use x25519_dalek::{EphemeralSecret, PublicKey};
mod b2e_process;
mod tls;

use crate::{
    auth::verify_user,
    broker::{broker_loop, ACCEPT_FREE},
    ipv6::{configure_ipv6_routing, EyeballDialer},
    proxy::proxy_stream,
    ratelimit::{get_ratelimiter, RateLimiter},
    tasklimit::new_task_until_death,
    CONFIG_FILE, SIGNING_SECRET,
};

pub async fn listen_main() -> anyhow::Result<()> {
    configure_ipv6_routing().await?;
    let c2e = c2e_loop();
    let b2e = b2e_loop();
    let broker = broker_loop();
    c2e.race(broker).race(b2e).await
}

async fn c2e_loop() -> anyhow::Result<()> {
    let listener = TcpListener::bind(CONFIG_FILE.wait().c2e_listen).await?;
    let mut listener = sillad_conntest::ConnTestListener::new(listener);
    loop {
        let c2e_raw = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::error!(err = debug(err), "error accepting");
                continue;
            }
        };

        let test_addr = async {
            let remote_addr: SocketAddr = c2e_raw.remote_addr().unwrap().parse()?;
            if let SocketAddr::V4(remote_addr) = remote_addr {
                let (asn, country) = ip_to_asn_country(*remote_addr.ip()).await?;
                tracing::trace!(asn, country, remote_addr = display(remote_addr), "got ASN");
                if CONFIG_FILE.wait().country_blacklist.contains(&country) {
                    anyhow::bail!(
                        "rejected connection from {remote_addr}/AS{asn} in blacklisted country {country}"
                    )
                }
            }
            anyhow::Ok(())
        };
        if let Err(err) = test_addr.await {
            tracing::warn!(err = debug(err), "rejected a direct connection");
            continue;
        }
        smolscale::spawn(handle_client(c2e_raw)).detach()
    }
}

async fn b2e_loop() -> anyhow::Result<()> {
    let mut listener = TcpListener::bind(CONFIG_FILE.wait().b2e_listen).await?;
    let b2e_table: Cache<B2eMetadata, Sender<picomux::Stream>> = Cache::builder()
        .time_to_idle(Duration::from_secs(1200))
        .build();
    loop {
        let b2e_raw = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::error!(err = debug(err), "error accepting");
                continue;
            }
        };
        let bridge_addr = b2e_raw
            .remote_addr()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let (read, write) = b2e_raw.split();
        let mut b2e_mux = PicoMux::new(read, write);
        b2e_mux.set_liveness(LivenessConfig {
            ping_interval: Duration::from_secs(3600),
            timeout: Duration::from_secs(3600),
        });
        let b2e_table = b2e_table.clone();
        smolscale::spawn::<anyhow::Result<()>>(async move {
            loop {
                let lala = b2e_mux.accept().await?;
                let b2e_metadata: B2eMetadata = stdcode::deserialize(lala.metadata())?;
                tracing::trace!(
                    bridge_addr = display(&bridge_addr),
                    "accepting b2e with metadata"
                );

                let send = b2e_table
                    .get_with(b2e_metadata.clone(), async {
                        tracing::debug!(
                            bridge_addr = display(&bridge_addr),
                            table_length = display(b2e_table.entry_count()),
                            "creating new b2e metadata"
                        );
                        let (send, recv) = tachyonix::channel(1);
                        smolscale::spawn(b2e_process::b2e_process(b2e_metadata, recv)).detach();
                        send
                    })
                    .await;
                send.send(lala).await.ok().context("could not accept")?;
            }
        })
        .detach()
    }
}

async fn handle_client(mut client: impl Pipe) -> anyhow::Result<()> {
    // execute the authentication
    let client_hello: ClientHello = stdcode::deserialize(&read_prepend_length(&mut client).await?)?;

    let keys: Option<([u8; 32], [u8; 32])>;
    let exit_hello_inner: ExitHelloInner = match client_hello.crypt_hello {
        ClientCryptHello::SharedSecretChallenge(key) => {
            let real_ss = client.shared_secret().context("no shared secret")?;
            let mac = blake3::keyed_hash(&key, real_ss);
            keys = None;
            ExitHelloInner::SharedSecretResponse(mac)
        }
        ClientCryptHello::X25519(their_epk) => {
            let my_esk = EphemeralSecret::random_from_rng(rand::thread_rng());
            let my_epk = PublicKey::from(&my_esk);
            let shared_secret = my_esk.diffie_hellman(&their_epk);
            let read_key = blake3::derive_key("c2e", shared_secret.as_bytes());
            let write_key = blake3::derive_key("e2c", shared_secret.as_bytes());
            keys = Some((read_key, write_key));
            ExitHelloInner::X25519(my_epk)
        }
    };

    let mut is_free = false;
    let ratelimit = if CONFIG_FILE.wait().broker.is_some() {
        let (level, token, sig): (AccountLevel, ClientToken, UnblindedSignature) =
            stdcode::deserialize(&client_hello.credentials)
                .context("cannot deserialize credentials")?;
        if level == AccountLevel::Free && !ACCEPT_FREE.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("free users rejected here")
        }
        verify_user(level, token, sig).await.inspect_err(|e| {
            tracing::warn!(err = debug(e), "**** BAD BAD bad token received ***")
        })?;
        is_free = level == AccountLevel::Free;
        get_ratelimiter(level, token).await
    } else {
        RateLimiter::unlimited()
    };

    let exit_hello = ExitHello {
        inner: exit_hello_inner.clone(),
        signature: SIGNING_SECRET.sign(&(client_hello, exit_hello_inner).stdcode()),
    };
    write_prepend_length(&exit_hello.stdcode(), &mut client).await?;

    let client = if let Some((read_key, write_key)) = keys {
        EitherPipe::Left(ClientExitCryptPipe::new(client, read_key, write_key))
    } else {
        EitherPipe::Right(client)
    };

    let (client_read, client_write) = client.split();
    let mux = PicoMux::new(client_read, client_write);
    mux.set_debloat(true);

    let mut sess_metadata = Arc::new(serde_json::Value::Null);
    let dialer = EyeballDialer::new();
    loop {
        let stream = mux.accept().await?;
        let metadata = String::from_utf8_lossy(stream.metadata()).to_string();
        if let Ok(new_sess_metadata) = serde_json::from_str::<serde_json::Value>(&metadata) {
            sess_metadata = Arc::new(new_sess_metadata);
            continue;
        }
        let sess_metadata = sess_metadata.clone();
        let dialer = dialer.clone();
        smolscale::spawn(
            proxy_stream(
                dialer,
                sess_metadata.clone(),
                ratelimit.clone(),
                stream,
                is_free,
            )
            .race(new_task_until_death(Duration::from_secs(1)))
            .map_err(|e| tracing::trace!(err = debug(e), "stream died with")),
        )
        .detach();
    }
}
