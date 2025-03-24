use anyctx::AnyCtx;
use anyhow::Context;
use bytes::Bytes;
use clone_macro::clone;
use ed25519_dalek::VerifyingKey;
use futures_util::{future::join_all, AsyncReadExt as _};
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use nursery_macro::nursery;

use picomux::{LivenessConfig, PicoMux};
use rand::Rng;
use sillad::{dialer::Dialer as _, EitherPipe, Pipe};
use smol::future::FutureExt as _;
use smol_timeout2::TimeoutExt;
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use stdcode::StdcodeSerializeExt;

use crate::{
    auth::get_connect_token, china::is_chinese_host, client::CtxField, control_prot::{ConnectedInfo, CURRENT_CONN_INFO}, route::get_dialer, spoof_dns::fake_dns_backtranslate, stats::{stat_incr_num, stat_set_num}, traffcount::TRAFF_COUNT, vpn::smart_vpn_whitelist, ConnInfo
};

use super::Config;

pub async fn open_conn(
    ctx: &AnyCtx<Config>,
    protocol: &str,
    dest_addr: &str,
) -> anyhow::Result<Box<dyn sillad::Pipe>> {
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

    if let Some((dest_host, _)) = dest_addr.rsplit_once(":") {
        if whitelist_host(ctx, dest_host) {
            let addrs = smol::net::resolve(&dest_addr).await?;
            for addr in addrs.iter() {
                smart_vpn_whitelist(ctx, addr.ip());
            }
            tracing::debug!(
                dest_addr = debug(dest_addr),
                "passing through whitelisted address"
            );
            return Ok(sillad::tcp::HappyEyeballsTcpDialer(addrs).dial().await?);
        }
    }

    let (send, recv) = oneshot::channel();
    let elem = (format!("{protocol}${dest_addr}"), send);
    let _ = ctx.get(CONN_REQ_CHAN).0.send(elem).await;
    let mut conn = recv.await?;
    let ctx = ctx.clone();
    conn.set_on_read(clone!([ctx], move |n| {
        stat_incr_num(&ctx, "total_rx_bytes", n as _);
        ctx.get(TRAFF_COUNT).write().unwrap().incr(n as _);
    }));
    conn.set_on_write(clone!([ctx], move |n| {
        stat_incr_num(&ctx, "total_tx_bytes", n as _);
        ctx.get(TRAFF_COUNT).write().unwrap().incr(n as _);
    }));
    Ok(Box::new(conn))
}


fn whitelist_host(ctx: &AnyCtx<Config>, host: &str) -> bool {
    if host.is_empty() || host.contains("[") {
        return false;
    }
    if let Ok(ip) = IpAddr::from_str(host) {
        match ip {
            IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_loopback(),
        }
    } else {
        if ctx.init().passthrough_china {
            if let Some(domain) = psl::domain_str(host) {
                if is_chinese_host(domain) {
                    return true;
                }
            }
        }
        match psl::suffix(host.as_bytes()) {
            None => false,
            Some(suf) => !suf.is_known(),
        }
    }
}

type ChanElem = (String, oneshot::Sender<picomux::Stream>);

static CONN_REQ_CHAN: CtxField<(
    smol::channel::Sender<ChanElem>,
    smol::channel::Receiver<ChanElem>,
)> = |_| {
    let (a, b) = smol::channel::unbounded();
    (a, b)
};

pub static CONCURRENCY: usize = 3;

#[tracing::instrument(skip_all)]
pub async fn client_inner(ctx: AnyCtx<Config>) -> Infallible {
    tracing::info!("(re)starting main logic");
    *ctx.get(CURRENT_CONN_INFO).lock() = ConnInfo::Connecting;

    let start = Instant::now();

    tracing::debug!(elapsed = debug(start.elapsed()), "raw dialer constructed");

    #[allow(unreachable_code)]
    let instance_thread = |instance| {

        let ctx = ctx.clone();
        smolscale::spawn(async move {
            loop {
                let once = async {
                    *ctx.get(CURRENT_CONN_INFO).lock() = ConnInfo::Connecting;
                    let (authed_pipe, exit) = async {
                        let (pubkey, exit, raw_dialer) = get_dialer(&ctx).await?;
                        let start = Instant::now();
                        let raw_pipe = raw_dialer.dial().await.context("could not dial")?;
                        tracing::debug!(
                            elapsed = debug(start.elapsed()),
                            protocol = raw_pipe.protocol(),
                            "dial completed"
                        );

                        let authed_pipe = client_auth(&ctx, raw_pipe, pubkey)
                            .await
                            .context("could not client auth")?;

                        tracing::debug!(
                            elapsed = debug(start.elapsed()),
                            "authentication done, starting mux system"
                        );
                        anyhow::Ok((authed_pipe, exit))
                    }
                    .timeout(Duration::from_secs(30))
                    .await
                    .context("overall dial/mux/auth timeout")??;

                    *ctx.get(CURRENT_CONN_INFO).lock() = ConnInfo::Connected(ConnectedInfo {
                        protocol: authed_pipe.protocol().to_string(),
                        bridge: authed_pipe
                            .remote_addr()
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                        exit: exit.clone(),
                    });
                    let addr: SocketAddr = authed_pipe.remote_addr().unwrap_or("").parse()?;
                    proxy_loop(ctx.clone(), authed_pipe, instance)
                        .await
                        .context(format!("inner connection to {addr} failed"))

                };
                if let Err(err) = once.await {
                    let wait_time = Duration::from_secs_f64(rand::thread_rng().gen_range(1.0..10.0));
                    tracing::warn!(instance, err = debug(err), wait_time=debug(wait_time), "individual client thread failed");
                    smol::Timer::after(wait_time).await;
                }
            }
        })
    };

    let _refresh = {
            let ctx = ctx.clone();
            smolscale::spawn(async move {
            loop {
                let sleep_secs = rand::thread_rng().gen_range(300..3600);
                smol::Timer::after(Duration::from_secs(sleep_secs)).await;
                let _ = get_dialer(&ctx).await;
            }
        })
    };

    join_all((0..CONCURRENCY).map(instance_thread)).await;
    unreachable!()
}

#[tracing::instrument(skip_all, fields(instance=instance, server=display(authed_pipe.remote_addr().unwrap_or("(none)"))))]
async fn proxy_loop(
    ctx: AnyCtx<Config>,
    authed_pipe: impl Pipe,
    instance: usize,
) -> anyhow::Result<()> {
    let (read, write) = authed_pipe.split();
    let mut mux = PicoMux::new(read, write);
    mux.set_liveness(LivenessConfig {
        ping_interval: Duration::from_secs(1800),
        timeout: Duration::from_secs(3),
    });
    let mux = Arc::new(mux);

    // we first register the session metadata
    mux.open(&serde_json::to_vec(&ctx.init().sess_metadata)?).await?;

    async {
        nursery!({
            loop {
                let mux = mux.clone();
                let ctx = ctx.clone();
                let (remote_addr, send_back) = ctx.get(CONN_REQ_CHAN).1.recv().await?;
                if let Some(latency) = mux.last_latency() {
                    stat_set_num(&ctx, "ping", latency.as_secs_f64());
                }
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
        let (level, token, sig) = get_connect_token(ctx)
            .await
            .context("cannot get connect token")?;
        tracing::info!(level=debug(level), "authentication with a connect token");
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
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)
                    .context("cannot deserialize exit hello")?;
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
                stdcode::deserialize(&read_prepend_length(&mut pipe).await?)
                    .context("could not deserialize exit hello")?;
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
