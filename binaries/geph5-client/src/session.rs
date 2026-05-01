use anyctx::AnyCtx;
use anyhow::Context;
use bytes::Bytes;
use clone_macro::clone;
use ed25519_dalek::VerifyingKey;
use futures_intrusive::sync::ManualResetEvent;
use futures_util::AsyncReadExt as _;
use geph5_misc_rpc::{
    client_control::ConnectedInfo,
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length,
    tunnel_command::{RichTunnelCommand, RichTunnelResponse, TunnelCommand},
    write_prepend_length,
};

use picomux::{LivenessConfig, PicoMux};
use rand::Rng;
use sillad::{EitherPipe, Pipe, dialer::Dialer as _};
use slab::Slab;
use smol::future::FutureExt as _;
use smol_timeout2::TimeoutExt;
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use stdcode::StdcodeSerializeExt;

use crate::{
    auth::{IS_PLUS, get_connect_token},
    bw_accounting::{
        BwAccountingHandle, BwAccountingLoop, bw_accounting_client_loop, bw_accounting_pair,
        notify_bw_accounting,
    },
    china::is_chinese_host,
    client::CtxField,
    control_prot::CURRENT_CONNECTED_INFOS,
    get_dialer::get_dialer,
    spoof_dns::fake_dns_backtranslate,
    stats::{stat_incr_num, stat_set_num},
    traffcount::TRAFF_COUNT,
    vpn::smart_vpn_whitelist,
};

use super::Config;

const TARGET_SESSION_COUNT: usize = 1;

struct ConnectedSession {
    worker_id: usize,
    mux: Arc<PicoMux>,
    accounting: BwAccountingHandle,
    pending_opens: AtomicUsize,
    early_dead: Arc<ManualResetEvent>,
}

static CURRENT_SESSIONS: CtxField<parking_lot::Mutex<Slab<Arc<ConnectedSession>>>> =
    |_| parking_lot::Mutex::new(Slab::new());

static NEXT_SESSION_PICK: CtxField<AtomicUsize> = |_| AtomicUsize::new(0);

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

    if let Some((dest_host, _)) = dest_addr.rsplit_once(":")
        && whitelist_host(ctx, dest_host)
    {
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

    let cmd = RichTunnelCommand {
        protocol: protocol.to_string(),
        host: dest_addr.to_string(),
    };

    let session = wait_for_session(ctx).await;
    match open_tunnel_on_session(ctx, &session, cmd.clone()).await {
        Ok(conn) => {
            let mut conn = conn;
            let ctx = ctx.clone();
            let accounting = session.accounting.clone();
            conn.set_on_read(clone!([ctx, accounting], move |n| {
                notify_bw_accounting(&ctx, &accounting, n);
                stat_incr_num(&ctx, "total_rx_bytes", n as _);
                ctx.get(TRAFF_COUNT).write().unwrap().incr(n as _);
            }));
            conn.set_on_write(clone!([ctx, accounting], move |n| {
                notify_bw_accounting(&ctx, &accounting, n);
                stat_incr_num(&ctx, "total_tx_bytes", n as _);
                ctx.get(TRAFF_COUNT).write().unwrap().incr(n as _);
            }));
            Ok(Box::new(conn))
        }
        Err(err) => {
            tracing::warn!(err = debug(&err), "opening something on the session failed");
            Err(err)
        }
    }
}

async fn wait_for_session(ctx: &AnyCtx<Config>) -> Arc<ConnectedSession> {
    loop {
        if let Some(session) = select_session(ctx) {
            return session;
        }
        smol::Timer::after(Duration::from_millis(100)).await;
    }
}

fn select_session(ctx: &AnyCtx<Config>) -> Option<Arc<ConnectedSession>> {
    let mut sessions = ctx
        .get(CURRENT_SESSIONS)
        .lock()
        .iter()
        .filter(|(_, session)| session.mux.is_alive() && !session.early_dead.is_set())
        .map(|(_, session)| session.clone())
        .collect::<Vec<_>>();
    if sessions.is_empty() {
        return None;
    }

    let offset = ctx.get(NEXT_SESSION_PICK).fetch_add(1, Ordering::Relaxed) % sessions.len();
    sessions.rotate_left(offset);
    sessions.into_iter().min_by_key(|session| {
        session
            .pending_opens
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

#[tracing::instrument(
    skip(ctx, session, cmd),
    fields(
        worker_id = session.worker_id,
        protocol = %cmd.protocol,
        host = %cmd.host,
    )
)]
async fn open_tunnel_on_session(
    ctx: &AnyCtx<Config>,
    session: &Arc<ConnectedSession>,
    cmd: RichTunnelCommand,
) -> anyhow::Result<picomux::Stream> {
    session.pending_opens.fetch_add(1, Ordering::Relaxed);
    scopeguard::defer!({
        session.pending_opens.fetch_sub(1, Ordering::Relaxed);
    });

    if let Some(latency) = session.mux.last_latency() {
        stat_set_num(ctx, "ping", latency.as_secs_f64());
    }

    let cmd_str = TunnelCommand::Rich(cmd).to_string();
    let start = Instant::now();
    tracing::debug!(cmd_str = display(&cmd_str), "opening tunnel...");
    let mut stream = match session.mux.open(cmd_str.as_bytes()).await {
        Ok(stream) => stream,
        Err(err) => {
            session.early_dead.set();
            return Err(err).context("could not open stream on session");
        }
    };

    let open_response = geph5_misc_rpc::read_prepend_length(&mut stream)
        .timeout(Duration::from_secs(10))
        .await
        .ok_or_else(|| {
            session.early_dead.set();
            anyhow::anyhow!("timeout waiting for tunnel open response")
        })?;

    let resp: RichTunnelResponse = match open_response {
        Ok(resp) => match serde_json::from_slice(&resp) {
            Ok(resp) => resp,
            Err(err) => {
                session.early_dead.set();
                return Err(err).context("could not parse tunnel open response");
            }
        },
        Err(err) => {
            session.early_dead.set();
            return Err(err).context("could not read tunnel open response");
        }
    };
    let open_ms = match resp {
        RichTunnelResponse::Success { open_ms, .. } => open_ms,
        RichTunnelResponse::Fail(error) => {
            return Err(anyhow::anyhow!("exit failed to open tunnel: {error}"));
        }
    };
    tracing::debug!(
        cmd_str = display(&cmd_str),
        total_latency = start.elapsed().as_millis(),
        remote_latency = open_ms,
        "tunnel open"
    );
    stat_set_num(ctx, "ping", start.elapsed().as_secs_f64());
    Ok(stream)
}

#[tracing::instrument(skip_all)]
pub async fn run_session(ctx: AnyCtx<Config>) -> Infallible {
    tracing::info!("(re)starting main logic");
    let start = Instant::now();

    tracing::debug!(elapsed = debug(start.elapsed()), "raw dialer constructed");

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

    for worker_id in 0..TARGET_SESSION_COUNT {
        let ctx = ctx.clone();
        smolscale::spawn(session_worker(ctx, worker_id)).detach();
    }
    smol::future::pending().await
}

#[tracing::instrument(skip(ctx))]
async fn session_worker(ctx: AnyCtx<Config>, worker_id: usize) -> Infallible {
    let mut failures = 0.0f64;
    if worker_id > 0 {
        let jitter = rand::thread_rng().gen_range(0.0..10.0);
        smol::Timer::after(Duration::from_secs_f64(jitter)).await;
    }

    loop {
        let wait_time = Duration::from_secs_f64(
            (rand::thread_rng().gen_range(0.0..0.1) * failures.exp2()).min(120.0),
        );
        let timeout_time = Duration::from_secs_f64(
            (rand::thread_rng().gen_range(30.0..60.0) * failures.exp2()).min(120.0),
        );
        if let Err(err) = run_session_once(&ctx, worker_id, timeout_time, &mut failures).await {
            failures += 1.0;
            tracing::warn!(
                err = debug(&err),
                worker_id,
                wait_time = debug(wait_time),
                "individual client session failed..."
            );
            smol::Timer::after(wait_time).await;
            tracing::warn!(worker_id, wait_time = debug(wait_time), "retrying now!");
        } else {
            failures = 0.0;
        }
    }
}

fn whitelist_host(ctx: &AnyCtx<Config>, host: &str) -> bool {
    if host.is_empty() || host.contains("[") {
        return false;
    }
    if ctx.init().passthrough_china && is_chinese_host(host) {
        return true;
    }
    if let Ok(ip) = IpAddr::from_str(host) {
        match ip {
            IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_loopback(),
        }
    } else {
        false
    }
}

async fn run_session_once(
    ctx: &AnyCtx<Config>,
    worker_id: usize,
    timeout_time: Duration,
    failures: &mut f64,
) -> anyhow::Result<()> {
    let (authed_pipe, exit) = async {
        let (pubkey, exit, raw_dialer) = get_dialer(ctx).await?;
        let start = Instant::now();
        let raw_pipe = raw_dialer.dial().await.context("could not dial")?;
        tracing::debug!(
            elapsed = debug(start.elapsed()),
            protocol = raw_pipe.protocol(),
            "dial completed"
        );

        let authed_pipe = client_auth(ctx, raw_pipe, pubkey)
            .await
            .context("could not client auth")?;

        tracing::debug!(
            elapsed = debug(start.elapsed()),
            "authentication done, starting mux system"
        );
        anyhow::Ok((authed_pipe, exit))
    }
    .timeout(timeout_time)
    .await
    .context("overall dial/mux/auth timeout")??;

    let connected_info = ConnectedInfo {
        protocol: authed_pipe.protocol().to_string(),
        exit: exit.clone(),
        bridge: authed_pipe
            .remote_addr()
            .and_then(|addr| addr.parse::<SocketAddr>().ok()),
    };
    let addr: SocketAddr = authed_pipe.remote_addr().unwrap_or("").parse()?;
    let mux = start_mux(authed_pipe);
    let (accounting, accounting_loop) = bw_accounting_pair();
    let early_dead = Arc::new(ManualResetEvent::new(false));
    let session = Arc::new(ConnectedSession {
        worker_id,
        mux: mux.clone(),
        accounting,
        pending_opens: AtomicUsize::new(0),
        early_dead,
    });
    *failures = 0.0;

    // we first register the session metadata
    mux.open(&serde_json::to_vec(&ctx.init().sess_metadata)?)
        .await?;
    let connected_info_idx = ctx
        .get(CURRENT_CONNECTED_INFOS)
        .lock()
        .insert(connected_info);
    let session_idx = ctx.get(CURRENT_SESSIONS).lock().insert(session.clone());
    scopeguard::defer!({
        ctx.get(CURRENT_CONNECTED_INFOS)
            .lock()
            .remove(connected_info_idx);
        ctx.get(CURRENT_SESSIONS).lock().remove(session_idx);
    });
    proxy_loop(ctx.clone(), session, accounting_loop)
        .await
        .context(format!("inner connection to {addr} failed"))
}

pub async fn wait_until_tunnel_ready(ctx: &AnyCtx<Config>) {
    while ctx.get(CURRENT_CONNECTED_INFOS).lock().is_empty() {
        smol::Timer::after(Duration::from_millis(100)).await;
    }
}

fn start_mux(authed_pipe: impl Pipe) -> Arc<PicoMux> {
    let (read, write) = authed_pipe.split();
    let mut mux = PicoMux::new(read, write);
    mux.set_liveness(LivenessConfig {
        ping_interval: Duration::from_secs(1200),
        timeout: Duration::from_secs(120),
    });
    mux.set_debloat(true);
    Arc::new(mux)
}

#[tracing::instrument(skip_all)]
async fn proxy_loop(
    ctx: AnyCtx<Config>,
    session: Arc<ConnectedSession>,
    accounting_loop: BwAccountingLoop,
) -> anyhow::Result<()> {
    session
        .mux
        .wait_until_dead()
        .or(async {
            if ctx.get(IS_PLUS).load(Ordering::SeqCst) {
                bw_accounting_client_loop(
                    ctx.clone(),
                    session.mux.open(b"!bw-accounting-2").await?,
                    accounting_loop,
                )
                .await
            } else {
                smol::future::pending().await
            }
        })
        .or(async {
            session.early_dead.wait().await;
            tracing::warn!("dying due to an early-dead signal");
            anyhow::bail!("early dead")
        })
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
        tracing::info!(level = debug(level), "authentication with a connect token");
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
