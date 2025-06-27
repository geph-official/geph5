use std::{io::ErrorKind, net::SocketAddr, sync::Arc, time::Duration};

use async_io::Timer;
use async_lock::Semaphore;
use async_task::Task;
use async_trait::async_trait;
use futures_concurrency::future::Race;
use futures_util::{AsyncRead, AsyncWrite};

use parking_lot::Mutex;
use pin_project::pin_project;
use sillad::{Pipe, dialer::Dialer, listener::Listener};
use sosistab2::Pipe as _;
use sosistab2_obfsudp::{ObfsUdpPublic, ObfsUdpSecret};
use stdcode::StdcodeSerializeExt;
use virta::{StreamMessage, stream_state::StreamState};

/// Convert an ObfsUdp secret key to a public key.
pub fn sk_to_pk(sk: [u8; 32]) -> [u8; 32] {
    *ObfsUdpSecret::from_bytes(sk).to_public().as_bytes()
}

/// A dialer for obfuscated UDP streams.
pub struct ObfsUdpDialer {
    pub addr: SocketAddr,
    pub server_pk: [u8; 32],
}

#[async_trait]
impl Dialer for ObfsUdpDialer {
    type P = ObfsUdpPipe;

    async fn dial(&self) -> std::io::Result<Self::P> {
        let key: ObfsUdpPublic = ObfsUdpPublic::from_bytes(self.server_pk);
        let pipe = sosistab2_obfsudp::ObfsUdpPipe::connect(self.addr, key, "")
            .await
            .map_err(|e| std::io::Error::new(ErrorKind::BrokenPipe, e))?;
        let ticker = Arc::new(Semaphore::new(0));
        let (state, stream) = StreamState::new_pending({
            let ticker = ticker.clone();
            move || ticker.add_permits(1)
        });
        let state = Arc::new(Mutex::new(state));
        let (send_msg, recv_msg) = tachyonix::channel(100);
        let _ticker = smolscale::spawn(
            (
                stream_ticker(ticker, send_msg, state.clone()),
                pipe_shuffle(pipe, recv_msg, state),
            )
                .race(),
        );
        Ok(ObfsUdpPipe {
            inner: stream,
            _ticker,
        })
    }
}

/// A listener for obfuscated UDP streams.
pub struct ObfsUdpListener {
    inner: sosistab2_obfsudp::ObfsUdpListener,
}

impl ObfsUdpListener {
    pub async fn listen(bind: SocketAddr, server_sk: [u8; 32]) -> std::io::Result<Self> {
        let inner =
            sosistab2_obfsudp::ObfsUdpListener::bind(bind, ObfsUdpSecret::from_bytes(server_sk))
                .await
                .map_err(|e| std::io::Error::new(ErrorKind::BrokenPipe, e))?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl Listener for ObfsUdpListener {
    type P = ObfsUdpPipe;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        let pipe = self
            .inner
            .accept()
            .await
            .map_err(|e| std::io::Error::new(ErrorKind::BrokenPipe, e))?;
        let ticker = Arc::new(Semaphore::new(0));
        let (state, stream) = StreamState::new_established({
            let ticker = ticker.clone();
            move || ticker.add_permits(1)
        });
        let state = Arc::new(Mutex::new(state));
        let (send_msg, recv_msg) = tachyonix::channel(100);
        let _ticker = smolscale::spawn(
            (
                stream_ticker(ticker, send_msg, state.clone()),
                pipe_shuffle(pipe, recv_msg, state),
            )
                .race(),
        );
        Ok(ObfsUdpPipe {
            inner: stream,
            _ticker,
        })
    }
}

async fn pipe_shuffle(
    pipe: sosistab2_obfsudp::ObfsUdpPipe,
    mut recv_msg: tachyonix::Receiver<StreamMessage>,
    state: Arc<Mutex<StreamState>>,
) {
    let up_shuffle = async {
        while let Ok(msg) = recv_msg.recv().await {
            pipe.send(msg.stdcode().into());
        }
    };

    let dn_shuffle = async {
        while let Ok(msg) = pipe.recv().await {
            let msg = stdcode::deserialize(&msg);
            if let Ok(msg) = msg {
                state.lock().inject_incoming(msg);
            }
        }
    };

    (up_shuffle, dn_shuffle).race().await;
}

async fn stream_ticker(
    ticker: Arc<Semaphore>,
    send_msg: tachyonix::Sender<StreamMessage>,
    state: Arc<Mutex<StreamState>>,
) {
    state.lock().set_mss(1000);
    let mut timer = Timer::after(Duration::from_secs(10));
    loop {
        let tick_result = state.lock().tick(|message| {
            let _ = send_msg.try_send(message);
        });
        match tick_result {
            None => return,
            Some(instant) => {
                timer.set_at(instant);
                (
                    async {
                        (&mut timer).await;
                    },
                    async {
                        ticker.acquire().await.forget();
                    },
                )
                    .race()
                    .await
            }
        }
    }
}

/// An obfuscated UDP stream.
#[pin_project]
pub struct ObfsUdpPipe {
    #[pin]
    inner: virta::Stream,

    _ticker: Task<()>,
}

impl Pipe for ObfsUdpPipe {
    fn protocol(&self) -> &str {
        "obfsudp"
    }

    fn remote_addr(&self) -> Option<&str> {
        None
    }
}

impl AsyncRead for ObfsUdpPipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for ObfsUdpPipe {
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

impl ObfsUdpPipe {}
