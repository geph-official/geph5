use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, LazyLock,
    },
    time::Duration,
};

use anyhow::Context;
use async_channel::{Receiver, Sender};
use async_io_bufpool::pooled_read;
use async_trait::async_trait;

use futures_util::{AsyncRead, AsyncReadExt as _, AsyncWrite};
use geph5_misc_rpc::bridge::{B2eMetadata, BridgeControlProtocol, BridgeControlService};
use moka::future::Cache;
use once_cell::sync::Lazy;
use picomux::{PicoMux, Stream};
use rand::Rng;
use sillad::{dialer::Dialer, listener::Listener, tcp::TcpListener, Pipe};
use smol::future::FutureExt as _;
use smol::io::AsyncWriteExt;
use smol_timeout2::TimeoutExt;
use stdcode::StdcodeSerializeExt;
use tap::Tap;

use crate::asn_count::{self, incr_bytes_asn};

pub async fn listen_forward_loop(my_ip: IpAddr, listener: impl Listener) -> anyhow::Result<()> {
    let state = State { my_ip };
    nanorpc_sillad::rpc_serve(listener, BridgeControlService(state)).await?;
    Ok(())
}

#[allow(clippy::type_complexity)]
struct State {
    // b2e_dest => (metadata, task)
    my_ip: IpAddr,
}

#[async_trait]
impl BridgeControlProtocol for State {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr {
        static MAPPING: LazyLock<
            Cache<(SocketAddr, B2eMetadata), (SocketAddr, Arc<smol::Task<anyhow::Result<()>>>)>,
        > = LazyLock::new(|| {
            Cache::builder()
                .time_to_idle(Duration::from_secs(3600))
                .build()
        });

        MAPPING
            .get_with((b2e_dest, metadata.clone()), async {
                let listener = random_tcp_listener().await;
                let addr = listener
                    .local_addr()
                    .await
                    .tap_mut(|s| s.set_ip(self.my_ip));
                let task = smolscale::spawn(handle_one_listener(listener, b2e_dest, metadata));
                (addr, Arc::new(task))
            })
            .await
            .0
    }
}

async fn random_tcp_listener() -> TcpListener {
    loop {
        let rando = rand::thread_rng().gen_range(2048u16..65535);
        match TcpListener::bind(format!("0.0.0.0:{rando}").parse().unwrap()).await {
            Ok(listener) => return listener,
            Err(err) => {
                smol::Timer::after(Duration::from_millis(100)).await;
                tracing::warn!(rando, err = debug(err), "retrying a bind...")
            }
        }
    }
}

async fn handle_one_listener(
    mut listener: impl Listener,
    b2e_dest: SocketAddr,
    metadata: B2eMetadata,
) -> anyhow::Result<()> {
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    loop {
        let client_conn = listener.accept().await?;
        let count = COUNT.fetch_add(1, Ordering::Relaxed);

        let remote_ip = SocketAddr::from_str(client_conn.remote_addr().unwrap())
            .unwrap()
            .ip();
        let remote_asn = asn_count::ip_to_asn(remote_ip).await?;
        tracing::debug!(
            count,
            asn = remote_asn,
            b2e_dest = debug(b2e_dest),
            "handled a connection"
        );
        let metadata = metadata.clone();
        smolscale::spawn(async move {
            scopeguard::defer!({
                let count = COUNT.fetch_sub(1, Ordering::Relaxed);
                tracing::debug!(
                    count,
                    asn = remote_asn,
                    b2e_dest = debug(b2e_dest),
                    "closing a connection"
                );
            });
            let exit_conn = dial_pooled(b2e_dest, &metadata.stdcode())
                .await
                .inspect_err(|e| tracing::warn!("cannot dial pooled: {:?}", e))?;
            let (client_read, client_write) = client_conn.split();
            let (exit_read, exit_write) = exit_conn.split();
            io_copy_with_timeout(
                exit_read,
                client_write,
                remote_asn,
                Duration::from_secs(1800),
            )
            .race(io_copy_with_timeout(
                client_read,
                exit_write,
                remote_asn,
                Duration::from_secs(1800),
            ))
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
    asn: u32,
    timeout: Duration,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        match pooled_read(&mut reader).timeout(timeout).await {
            Some(Ok(buf)) => {
                if buf.is_empty() {
                    return Ok(());
                }
                writer.write_all(&buf).await?;

                incr_bytes_asn(asn, buf.len() as u64);
            }
            Some(Err(err)) => return Err(err),
            None => return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")),
        }
    }
}

async fn dial_pooled(b2e_dest: SocketAddr, metadata: &[u8]) -> anyhow::Result<picomux::Stream> {
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
    let stream = pool
        .connect(metadata)
        .timeout(Duration::from_secs(1))
        .await
        .context("timeout connecting through pool")?
        .context(format!("cannot open through mux, b2e_dest={b2e_dest}"))?;
    Ok(stream)
}

struct SinglePool {
    send: Sender<(Vec<u8>, oneshot::Sender<Stream>)>,
    live_count: Arc<AtomicUsize>,
    _tasks: Vec<smol::Task<()>>,
}

impl SinglePool {
    pub async fn create(dest: SocketAddr) -> anyhow::Result<Self> {
        let (send, recv) = async_channel::bounded(100);
        let live_count = Arc::new(AtomicUsize::new(0));
        let mut tasks = vec![];
        for _ in 0..32 {
            let recv = recv.clone();
            let live_count = live_count.clone();
            let task = smolscale::spawn(async move {
                loop {
                    let conn = sillad::tcp::TcpDialer { dest_addr: dest }.dial().await;
                    if let Ok(conn) = conn {
                        let (read, write) = conn.split();
                        let mux = PicoMux::new(read, write);
                        let recv = recv.clone();
                        live_count.fetch_add(1, Ordering::Relaxed);
                        scopeguard::defer!({
                            live_count.fetch_sub(1, Ordering::Relaxed);
                        });
                        if let Err(err) = remote_once(recv.clone(), &mux).await {
                            tracing::error!(dest = display(dest), "remote_once error: {}", err);
                        }
                    }
                    smol::Timer::after(Duration::from_secs(30)).await;
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
        let (metadata, back) = req.recv().await?;
        let stream = mux.open(&metadata).await?;
        back.send(stream).ok();
    }
}
