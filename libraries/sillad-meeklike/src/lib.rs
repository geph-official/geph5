mod crypto;
mod datagram;

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use async_io::Timer;
use async_task::Task;
use async_trait::async_trait;
use event_listener::Event;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use stdcode::StdcodeSerializeExt;
use virta::{stream_state::StreamState, StreamMessage};

use crate::{
    crypto::PresharedSecret,
    datagram::{DgConnection, DgListener},
};
use futures_lite::FutureExt;
use futures_util::io::{AsyncRead, AsyncWrite};

use pin_project::pin_project;
use sillad::{dialer::Dialer, listener::Listener, Pipe};

/// Configuration for Meeklike
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, Hash, PartialEq)]
pub struct MeeklikeConfig {
    pub max_inflight: usize,
    pub mss: usize,
    pub base64: bool,
}

impl Default for MeeklikeConfig {
    fn default() -> Self {
        Self {
            max_inflight: 50,
            mss: 3000,
            base64: false,
        }
    }
}

#[pin_project]
/// A meeklike "pipe" that takes a meeklike stuff.
pub struct MeeklikePipe {
    #[pin]
    inner: virta::Stream,

    _task: Task<()>,
}

impl AsyncRead for MeeklikePipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for MeeklikePipe {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().inner.poll_close(cx)
    }
}

impl Pipe for MeeklikePipe {
    fn protocol(&self) -> &str {
        "meeklike"
    }
    fn remote_addr(&self) -> Option<&str> {
        Some("0.0.0.0:0")
    }
}

pub struct MeeklikeDialer<D: Dialer> {
    pub inner: Arc<D>,
    pub key: [u8; 32],
    pub cfg: MeeklikeConfig,
}

#[async_trait]
impl<D: Dialer> Dialer for MeeklikeDialer<D> {
    type P = MeeklikePipe;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let stream_id: u128 = rand::random();
        let dg_conn = DgConnection::new(
            self.cfg,
            PresharedSecret::new(&self.key).into(),
            stream_id,
            self.inner.clone(),
        );
        let notify = Arc::new(Event::new());
        let (mut state, inner) = virta::stream_state::StreamState::new_pending({
            let notify = notify.clone();
            move || {
                notify.notify(1);
            }
        });
        state.set_mss(self.cfg.mss);
        let _task = smolscale::spawn(ticker(notify, state, dg_conn));
        inner.wait_connected().await?;
        Ok(MeeklikePipe { inner, _task })
    }
}

async fn ticker(notify: Arc<Event>, state: StreamState, dg_conn: DgConnection) {
    let state = Mutex::new(state);
    let up = async {
        let mut timer = Timer::after(Duration::from_secs(10));
        loop {
            let evt = notify.listen();
            let next = state.lock().tick(|b| dg_conn.send(b.stdcode().into()));
            if let Some(next) = next {
                timer.set_at(next);
                async {
                    (&mut timer).await;
                }
                .race(async {
                    evt.await;
                })
                .await
            } else {
                break;
            }
        }
        anyhow::Ok(())
    };

    let dn = async {
        loop {
            let bts = dg_conn
                .recv()
                .await
                .context("could not received from underlying")?;
            let msg: Result<StreamMessage, _> = stdcode::deserialize(&bts);
            match msg {
                Ok(msg) => {
                    state.lock().inject_incoming(msg);
                }
                Err(err) => {
                    tracing::warn!(err = debug(err), "error getting message")
                }
            }
        }
    };

    if let Err(err) = up.race(dn).await {
        tracing::warn!(err = debug(err), "ticker died abnormally")
    }
}

pub struct MeeklikeListener<L: Listener> {
    listener: DgListener,
    cfg: MeeklikeConfig,
    _phantom: std::marker::PhantomData<L>,
}

impl<L: Listener> MeeklikeListener<L> {
    pub fn new(inner: L, key: [u8; 32], cfg: MeeklikeConfig) -> Self {
        let listener = DgListener::new(cfg, PresharedSecret::new(&key).into(), inner);
        Self {
            listener,
            cfg,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<L: Listener> Listener for MeeklikeListener<L> {
    type P = MeeklikePipe;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        let dg_conn = self
            .listener
            .accept()
            .await
            .map_err(std::io::Error::other)?;
        let notify = Arc::new(Event::new());
        let (mut state, inner) = virta::stream_state::StreamState::new_established({
            let notify = notify.clone();
            move || {
                notify.notify(1);
            }
        });
        state.set_mss(self.cfg.mss);
        let _task = smolscale::spawn(ticker(notify, state, dg_conn));
        Ok(MeeklikePipe { inner, _task })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{AsyncReadExt, AsyncWriteExt};
    use sillad::tcp::{TcpDialer, TcpListener};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn ping_pong() {
        smolscale::block_on(async {
            let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().await;
            let key = [7u8; 32];
            let mut meek_listener = MeeklikeListener::new(listener, key, Default::default());

            let dialer = MeeklikeDialer {
                inner: TcpDialer { dest_addr: addr }.into(),
                key,
                cfg: Default::default(),
            };

            let server = smolscale::spawn(async move {
                let mut pipe = meek_listener.accept().await.unwrap();
                let mut buf = [0u8; 4];
                pipe.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ping");
                pipe.write_all(b"pong").await.unwrap();
                pipe.flush().await.unwrap();
            });

            let client = smolscale::spawn(async move {
                let mut pipe = dialer.dial().await.unwrap();
                pipe.write_all(b"ping").await.unwrap();
                pipe.flush().await.unwrap();
                let mut buf = [0u8; 4];
                pipe.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"pong");
            });

            server.await;
            client.await;
        });
    }
}
