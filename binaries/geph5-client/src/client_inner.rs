use anyctx::AnyCtx;
use anyhow::Context;
use bytes::Bytes;
use clone_macro::clone;
use ed25519_dalek::VerifyingKey;
use futures_util::{future::try_join_all, AsyncReadExt as _};
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use nursery_macro::nursery;
use picomux::{LivenessConfig, PicoMux};
use rand::Rng;
use sillad::{
    dialer::{Dialer as _, DynDialer},
    EitherPipe, Pipe,
};
use smol::future::FutureExt as _;
use smol_timeout::TimeoutExt;
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use stdcode::StdcodeSerializeExt;

use crate::{
    auth::get_connect_token,
    client::CtxField,
    route::{deprioritize_route, get_dialer},
    stats::stat_incr_num,
    vpn::fake_dns_backtranslate,
};

use super::Config;

pub async fn open_conn(
    ctx: &AnyCtx<Config>,
    protocol: &str,
    dest_addr: &str,
) -> anyhow::Result<picomux::Stream> {
    let dest_addr = if let Ok(sock_addr) = SocketAddr::from_str(dest_addr) {
        if let IpAddr::V4(v4) = sock_addr.ip() {
            if let Some(orig) = fake_dns_backtranslate(ctx, v4) {
                format!("{orig}:{}", sock_addr.port())
            } else {
                dest_addr.to_string()
            }
        } else {
            dest_addr.to_string()
        }
    } else {
        dest_addr.to_string()
    };

    let (send, recv) = oneshot::channel();
    let elem = (format!("{protocol}${dest_addr}"), send);
    let _ = ctx.get(CONN_REQ_CHAN).0.send(elem).await;
    let mut conn = recv.await?;
    let ctx = ctx.clone();
    conn.set_on_read(clone!([ctx], move |n| {
        stat_incr_num(&ctx, "total_rx_bytes", n as _)
    }));
    conn.set_on_write(clone!([ctx], move |n| {
        stat_incr_num(&ctx, "total_tx_bytes", n as _)
    }));
    Ok(conn)
}

type ChanElem = (String, oneshot::Sender<picomux::Stream>);

static CONN_REQ_CHAN: CtxField<(
    smol::channel::Sender<ChanElem>,
    smol::channel::Receiver<ChanElem>,
)> = |_| smol::channel::unbounded();

static COUNTER: AtomicU64 = AtomicU64::new(0);

static CONCURRENCY: usize = 6;

#[tracing::instrument(skip_all, fields(instance=COUNTER.fetch_add(1, Ordering::Relaxed)))]
pub async fn client_once(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    tracing::info!("(re)starting main logic");

    static DIALER: CtxField<smol::lock::Mutex<Option<(VerifyingKey, DynDialer)>>> =
        |_| smol::lock::Mutex::new(None);

    let start = Instant::now();
    {
        let mut dialer = ctx.get(DIALER).lock().await;
        if dialer.is_none() {
            let (pubkey, raw_dialer) = get_dialer(&ctx).await?;
            *dialer = Some((pubkey, raw_dialer));
        }
    }

    let dial_refresh = async {
        loop {
            let secs = rand::thread_rng().gen_range(500..1000);
            tracing::info!(secs, "waiting until refresh");
            smol::Timer::after(Duration::from_secs(secs)).await;
            tracing::info!("refreshing dialer");
            let (pubkey, raw_dialer) = get_dialer(&ctx).await?;
            *ctx.get(DIALER).lock().await = Some((pubkey, raw_dialer));
        }
    };

    let (pubkey, raw_dialer) = ctx.get(DIALER).lock().await.as_ref().unwrap().clone();

    tracing::debug!(elapsed = debug(start.elapsed()), "raw dialer constructed");

    let once = || async {
        let authed_pipe = async {
            let raw_pipe = raw_dialer.dial().await?;
            tracing::debug!(
                elapsed = debug(start.elapsed()),
                protocol = raw_pipe.protocol(),
                "dial completed"
            );
            let died = AtomicBool::new(true);
            let addr: SocketAddr = raw_pipe.remote_addr().unwrap_or("").parse()?;
            scopeguard::defer!({
                {
                    if died.load(Ordering::SeqCst) {
                        tracing::debug!(addr = display(addr), "deprioritizing route");
                        deprioritize_route(addr);
                    }
                }
            });
            let authed_pipe = client_auth(&ctx, raw_pipe, pubkey).await?;
            died.store(false, Ordering::SeqCst);
            tracing::debug!(
                elapsed = debug(start.elapsed()),
                "authentication done, starting mux system"
            );
            anyhow::Ok(authed_pipe)
        }
        .timeout(Duration::from_secs(15))
        .await
        .context("overall dial/mux/auth timeout")??;
        smolscale::spawn(client_inner(ctx.clone(), authed_pipe)).await?;
        anyhow::Ok(())
    };

    try_join_all((0..CONCURRENCY).map(|_| once()))
        .or(dial_refresh)
        .await?;
    Ok(())
}

#[tracing::instrument(skip_all, fields(server=display(authed_pipe.remote_addr().unwrap_or("(none)"))))]
async fn client_inner(ctx: AnyCtx<Config>, authed_pipe: impl Pipe) -> anyhow::Result<()> {
    let (read, write) = authed_pipe.split();
    let mut mux = PicoMux::new(read, write);
    mux.set_liveness(LivenessConfig {
        ping_interval: Duration::from_secs(120),
        timeout: Duration::from_secs(10),
    });
    let mux = Arc::new(mux);

    async {
        nursery!({
            loop {
                let mux = mux.clone();
                let ctx = ctx.clone();
                let (remote_addr, send_back) = ctx.get(CONN_REQ_CHAN).1.recv().await?;
                smol::future::yield_now().await;
                spawn!(async move {
                    tracing::debug!(remote_addr = display(&remote_addr), "opening tunnel");
                    let stream = mux.open(remote_addr.as_bytes()).await;
                    match stream {
                        Ok(stream) => {
                            let _ = send_back.send(stream);
                        }
                        Err(err) => {
                            tracing::warn!(remote_addr = display(&remote_addr), err = debug(&err), "session is dead, hot-potatoing the connection request to somebody else");
                            let _ = ctx.get(CONN_REQ_CHAN).0.try_send((remote_addr, send_back));
                        }
                    }
                    anyhow::Ok(())
                })
                .detach();
            }
        })
    }.or(mux.wait_until_dead())
    .await
}

#[tracing::instrument(skip_all, fields(pubkey = hex::encode(pubkey.as_bytes())))]
async fn client_auth(
    ctx: &AnyCtx<Config>,
    mut pipe: impl Pipe,
    pubkey: VerifyingKey,
) -> anyhow::Result<impl Pipe> {
    let server = pipe.remote_addr().unwrap_or("").to_string();

    let credentials = if ctx.init().broker.is_none() {
        Bytes::new()
    } else {
        let (level, token, sig) = get_connect_token(ctx).await?;
        (level, token, sig).stdcode().into()
    };
    match pipe.shared_secret().map(|s| s.to_owned()) {
        Some(ss) => {
            tracing::debug!(server, "using shared secret for authentication");
            let challenge = rand::random();
            let client_hello = ClientHello {
                credentials,
                crypt_hello: ClientCryptHello::SharedSecretChallenge(challenge),
            };
            write_prepend_length(&client_hello.stdcode(), &mut pipe).await?;

            let mac = blake3::keyed_hash(&challenge, &ss);
            let exit_response: ExitHello =
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)?;
            match exit_response.inner {
                ExitHelloInner::SharedSecretResponse(response_mac) => {
                    if mac == response_mac {
                        tracing::debug!(server, "authentication successful with shared secret");
                        Ok(EitherPipe::Left(pipe))
                    } else {
                        anyhow::bail!("authentication failed with shared secret");
                    }
                }
                _ => anyhow::bail!("unexpected response from server"),
            }
        }
        None => {
            tracing::debug!(server, "requiring full authentication");
            let my_esk = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
            let client_hello = ClientHello {
                credentials,
                crypt_hello: ClientCryptHello::X25519((&my_esk).into()),
            };
            write_prepend_length(&client_hello.stdcode(), &mut pipe).await?;
            tracing::trace!(server, "wrote client hello");
            let exit_hello: ExitHello =
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)?;
            tracing::trace!(server, "received exit hello");
            // verify the exit hello
            let signed_value = (&client_hello, &exit_hello.inner).stdcode();
            pubkey
                .verify_strict(&signed_value, &exit_hello.signature)
                .context("exit hello failed validation")?;
            match exit_hello.inner {
                ExitHelloInner::Reject(reason) => {
                    anyhow::bail!("exit rejected our authentication attempt: {reason}")
                }
                ExitHelloInner::SharedSecretResponse(_) => {
                    anyhow::bail!(
                        "exit sent a shared-secret response to our full authentication request"
                    )
                }
                ExitHelloInner::X25519(their_epk) => {
                    let shared_secret = my_esk.diffie_hellman(&their_epk);
                    let read_key = blake3::derive_key("e2c", shared_secret.as_bytes());
                    let write_key = blake3::derive_key("c2e", shared_secret.as_bytes());
                    Ok(EitherPipe::Right(ClientExitCryptPipe::new(
                        pipe, read_key, write_key,
                    )))
                }
            }
        }
    }
}
