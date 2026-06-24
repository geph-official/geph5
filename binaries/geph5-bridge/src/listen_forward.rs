use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::Context;
use async_channel::{Receiver, Sender};
use async_trait::async_trait;
use futures_concurrency::future::Race;
use geph5_misc_rpc::bridge::{
    B2eMetadata, BridgeControlProtocol, BridgeControlService, ObfsProtocol,
};
use geph5_rt::{TimeoutExt, pooled_read};
use moka::future::Cache;
use once_cell::sync::Lazy;
use picomux::{LivenessConfig, PicoMux, Stream};
use rand::Rng;
use sillad::{Pipe, dialer::Dialer, listener::Listener, tcp::TcpListener};
use stdcode::StdcodeSerializeExt;
use tap::Tap;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::{
    asn_count::{self, incr_bytes_asn},
    ratelimit::BridgeRateLimiter,
};

pub async fn listen_forward_loop(
    my_ip: IpAddr,
    pool: String,
    listener: impl Listener,
    rate_limiter: BridgeRateLimiter,
) -> anyhow::Result<()> {
    let state = State {
        my_ip,
        pool,
        rate_limiter,
    };
    nanorpc_sillad::rpc_serve(listener, BridgeControlService(state)).await?;
    Ok(())
}

#[allow(clippy::type_complexity)]
struct State {
    // b2e_dest => (metadata, task)
    my_ip: IpAddr,
    pool: String,
    rate_limiter: BridgeRateLimiter,
}

#[async_trait]
impl BridgeControlProtocol for State {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr {
        static MAPPING: LazyLock<
            Cache<
                (IpAddr, SocketAddr, ObfsProtocol),
                (SocketAddr, Arc<geph5_rt::Task<anyhow::Result<()>>>),
            >,
        > = LazyLock::new(|| {
            Cache::builder()
                .time_to_idle(Duration::from_secs(3600))
                .build()
        });
        let protocol = metadata.protocol;

        MAPPING
            .get_with((self.my_ip, b2e_dest, protocol.clone()), async {
                let listener = random_tcp_listener(self.my_ip).await;
                let addr = listener
                    .local_addr()
                    .await
                    .tap_mut(|s| s.set_ip(self.my_ip));
                let task = geph5_rt::spawn(handle_one_listener(
                    listener,
                    b2e_dest,
                    protocol,
                    self.pool.clone(),
                    self.rate_limiter.clone(),
                ));
                (addr, Arc::new(task))
            })
            .await
            .0
    }
}

async fn random_tcp_listener(my_ip: IpAddr) -> TcpListener {
    loop {
        let rando = rand::thread_rng().gen_range(2048u16..65535);
        let bind_addr = SocketAddr::new(
            match my_ip {
                IpAddr::V4(_) => "0.0.0.0".parse().unwrap(),
                IpAddr::V6(_) => "::".parse().unwrap(),
            },
            rando,
        );
        match TcpListener::bind_with_v6_only(bind_addr, my_ip.is_ipv6()).await {
            Ok(listener) => return listener,
            Err(err) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                tracing::warn!(
                    rando,
                    bind_addr = display(bind_addr),
                    err = debug(err),
                    "retrying a bind..."
                )
            }
        }
    }
}

async fn handle_one_listener(
    mut listener: impl Listener,
    b2e_dest: SocketAddr,
    protocol: ObfsProtocol,
    pool: String,
    rate_limiter: BridgeRateLimiter,
) -> anyhow::Result<()> {
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    loop {
        let client_conn = listener.accept().await?;
        let count = COUNT.fetch_add(1, Ordering::Relaxed);

        let remote_ip = SocketAddr::from_str(client_conn.remote_addr().unwrap())
            .unwrap()
            .ip();
        let protocol = protocol.clone();
        let pool = pool.clone();
        let rate_limiter = rate_limiter.clone();
        geph5_rt::spawn(async move {
            let remote_asn = asn_count::ip_to_asn(remote_ip).await.unwrap_or(0);
            tracing::trace!(
                count,
                asn = remote_asn,
                b2e_dest = debug(b2e_dest),
                "handled a connection"
            );
            scopeguard::defer!({
                let count = COUNT.fetch_sub(1, Ordering::Relaxed);
                tracing::trace!(
                    count,
                    asn = remote_asn,
                    b2e_dest = debug(b2e_dest),
                    "closing a connection"
                );
            });
            let exit_conn = dial_pooled(b2e_dest, &protocol)
                .await
                .inspect_err(|e| tracing::warn!("cannot dial pooled: {:?}", e))?;
            let (client_read, client_write) = tokio::io::split(client_conn);
            let (exit_read, exit_write) = tokio::io::split(exit_conn);
            (
                io_copy_with_timeout(
                    exit_read,
                    client_write,
                    rate_limiter.clone(),
                    &pool,
                    remote_asn,
                    Duration::from_secs(1800),
                ),
                io_copy_with_timeout(
                    client_read,
                    exit_write,
                    rate_limiter,
                    &pool,
                    remote_asn,
                    Duration::from_secs(1800),
                ),
            )
                .race()
                .await?;
            anyhow::Ok(())
        })
        .detach();
    }
}

/// Copies data between a reader and a writer with a timeout.
pub async fn io_copy_with_timeout<R, W>(
    mut reader: R,
    mut writer: W,
    rate_limiter: BridgeRateLimiter,
    pool: &str,
    asn: u32,
    timeout: Duration,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        match pooled_read(&mut reader, 8192).timeout(timeout).await {
            // `None` (inner) signals EOF in async-io-bufpool 0.2.
            Some(Ok(Some(buf))) => {
                rate_limiter.wait(buf.len()).await;
                writer.write_all(&buf).await?;

                incr_bytes_asn(pool, asn, buf.len() as u64);
            }
            Some(Ok(None)) => return Ok(()),
            Some(Err(err)) => return Err(err),
            None => return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")),
        }
    }
}

fn encode_b2e_metadata(protocol: &ObfsProtocol) -> Vec<u8> {
    B2eMetadata {
        protocol: protocol.clone(),
        expiry: SystemTime::now() + Duration::from_secs(86400),
    }
    .stdcode()
}

async fn dial_pooled(
    b2e_dest: SocketAddr,
    protocol: &ObfsProtocol,
) -> anyhow::Result<picomux::Stream> {
    static POOLS: Lazy<Cache<SocketAddr, Arc<SinglePool>>> = Lazy::new(|| {
        Cache::builder()
            .time_to_idle(Duration::from_secs(3600 * 2))
            .build()
    });
    let pool = POOLS
        .try_get_with(b2e_dest, async {
            let pool = SinglePool::create(b2e_dest)
                .timeout(Duration::from_secs(1))
                .await
                .context("timed out creating pool")??;
            tracing::info!(b2e_dest = display(b2e_dest), "**** created a NEW pool ****");
            anyhow::Ok(Arc::new(pool))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let metadata = encode_b2e_metadata(protocol);
    let stream = pool
        .connect(&metadata)
        .timeout(Duration::from_secs(1))
        .await
        .context("timeout connecting through pool")?
        .context(format!("cannot open through mux, b2e_dest={b2e_dest}"))?;
    Ok(stream)
}

struct SinglePool {
    send: Sender<(Vec<u8>, oneshot::Sender<Stream>)>,
    live_count: Arc<AtomicUsize>,
    _tasks: Vec<geph5_rt::Task<()>>,
}

impl SinglePool {
    pub async fn create(dest: SocketAddr) -> anyhow::Result<Self> {
        let (send, recv) = async_channel::bounded(100);
        let live_count = Arc::new(AtomicUsize::new(0));
        let mut tasks = vec![];
        for _ in 0..16 {
            let recv = recv.clone();
            let live_count = live_count.clone();
            let task = geph5_rt::spawn(async move {
                loop {
                    let conn = sillad::tcp::TcpDialer { dest_addr: dest }.dial().await;
                    if let Ok(conn) = conn {
                        let (read, write) = tokio::io::split(conn);
                        let mut mux = PicoMux::new(read, write);
                        mux.set_liveness(LivenessConfig {
                            ping_interval: Duration::from_secs(10),
                            timeout: Duration::from_secs(10),
                        });
                        let recv = recv.clone();
                        live_count.fetch_add(1, Ordering::Relaxed);
                        scopeguard::defer!({
                            live_count.fetch_sub(1, Ordering::Relaxed);
                        });
                        if let Err(err) = remote_once(recv.clone(), &mux).await {
                            tracing::error!(dest = display(dest), "remote_once error: {}", err);
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
            tasks.push(task);
        }
        Ok(Self {
            send,
            live_count,
            _tasks: tasks,
        })
    }

    pub async fn connect(&self, metadata: &[u8]) -> anyhow::Result<Stream> {
        if self.live_count.load(Ordering::Relaxed) == 0 {
            anyhow::bail!("no live workers")
        }
        let (back, front) = oneshot::channel();
        self.send
            .send((metadata.to_vec(), back))
            .await
            .ok()
            .context("oh no underlying streams are dead")?;
        Ok(front.await?)
    }
}

async fn remote_once(
    req: Receiver<(Vec<u8>, oneshot::Sender<Stream>)>,
    mux: &PicoMux,
) -> anyhow::Result<()> {
    loop {
        let recv = async {
            req.recv()
                .await
                .map_err(|_| anyhow::anyhow!("request channel closed"))
        };
        let dead = async {
            mux.wait_until_dead().await?;
            anyhow::bail!("b2e mux died")
        };
        let (metadata, back) = (recv, dead).race().await?;
        let stream = mux.open(&metadata).await?;
        back.send(stream).ok();
    }
}
