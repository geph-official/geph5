use anyctx::AnyCtx;
use anyhow::Context;
use ed25519_dalek::VerifyingKey;
use futures_util::{future::try_join_all, AsyncReadExt as _};
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use nursery_macro::nursery;
use picomux::{LivenessConfig, PicoMux};
use sillad::{dialer::Dialer as _, EitherPipe, Pipe};
use smol::future::FutureExt as _;
use smol_timeout::TimeoutExt;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use stdcode::StdcodeSerializeExt;

use crate::{client::CtxField, route::get_dialer};

use super::Config;

pub async fn open_conn(ctx: &AnyCtx<Config>, dest_addr: &str) -> anyhow::Result<picomux::Stream> {
    let (send, recv) = oneshot::channel();
    let elem = (dest_addr.to_string(), send);
    let _ = ctx.get(CONN_REQ_CHAN).0.send(elem).await;
    Ok(recv.await?)
}

type ChanElem = (String, oneshot::Sender<picomux::Stream>);

static CONN_REQ_CHAN: CtxField<(
    smol::channel::Sender<ChanElem>,
    smol::channel::Receiver<ChanElem>,
)> = |_| smol::channel::unbounded();

static COUNTER: AtomicU64 = AtomicU64::new(0);

static CONCURRENCY: usize = 1;

#[tracing::instrument(skip_all, fields(instance=COUNTER.fetch_add(1, Ordering::Relaxed)))]
pub async fn client_once(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    tracing::info!("(re)starting main logic");
    let start = Instant::now();
    let (pubkey, raw_dialer) = get_dialer(&ctx).await?;
    tracing::debug!(elapsed = debug(start.elapsed()), "raw dialer constructed");

    let once = || async {
        let authed_pipe = async {
            let raw_pipe = raw_dialer.dial().await?;
            tracing::debug!(
                elapsed = debug(start.elapsed()),
                protocol = raw_pipe.protocol(),
                "dial completed"
            );
            let authed_pipe = client_auth(raw_pipe, pubkey).await?;
            tracing::debug!(
                elapsed = debug(start.elapsed()),
                "authentication done, starting mux system"
            );
            anyhow::Ok(authed_pipe)
        }
        .timeout(Duration::from_secs(60))
        .await
        .context("overall dial/mux/auth timeout")??;
        client_inner(ctx.clone(), authed_pipe).await?;
        anyhow::Ok(())
    };

    try_join_all((0..CONCURRENCY).map(|_| once())).await?;
    Ok(())
}

#[tracing::instrument(skip_all, fields(remote=display(authed_pipe.remote_addr().unwrap_or("(none)"))))]
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
                    tracing::debug!(remote_addr = display(&remote_addr), "connecting to remote");
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
async fn client_auth(mut pipe: impl Pipe, pubkey: VerifyingKey) -> anyhow::Result<impl Pipe> {
    match pipe.shared_secret().map(|s| s.to_owned()) {
        Some(ss) => {
            tracing::debug!("using shared secret for authentication");
            let challenge = rand::random();
            let client_hello = ClientHello {
                credentials: Default::default(), // no authentication support yet
                crypt_hello: ClientCryptHello::SharedSecretChallenge(challenge),
            };
            write_prepend_length(&client_hello.stdcode(), &mut pipe).await?;

            let mac = blake3::keyed_hash(&challenge, &ss);
            let exit_response: ExitHello =
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)?;
            match exit_response.inner {
                ExitHelloInner::SharedSecretResponse(response_mac) => {
                    if mac == response_mac {
                        tracing::debug!("authentication successful with shared secret");
                        Ok(EitherPipe::Left(pipe))
                    } else {
                        anyhow::bail!("authentication failed with shared secret");
                    }
                }
                _ => anyhow::bail!("unexpected response from server"),
            }
        }
        None => {
            tracing::debug!("requiring full authentication");
            let my_esk = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
            let client_hello = ClientHello {
                credentials: Default::default(), // no authentication support yet
                crypt_hello: ClientCryptHello::X25519((&my_esk).into()),
            };
            write_prepend_length(&client_hello.stdcode(), &mut pipe).await?;
            let exit_hello: ExitHello =
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)?;
            // verify the exit hello
            let signed_value = (&client_hello, &exit_hello.inner).stdcode();
            pubkey
                .verify_strict(&signed_value, &exit_hello.signature)
                .context("exit hello failed validation")?;
            match exit_hello.inner {
                ExitHelloInner::Reject(reason) => {
                    anyhow::bail!("exit rejected our authentication attempt: {reason}")
                }
                ExitHelloInner::SharedSecretResponse(_) => anyhow::bail!(
                    "exit sent a shared-secret response to our full authentication request"
                ),
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
