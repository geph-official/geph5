use std::{
    io::{Error, ErrorKind},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use async_channel::{Receiver, Sender};
use blake3::derive_key;
use bytes::Bytes;
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit};
use dashmap::DashMap;
use futures_util::{io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt}};
use hyper::{body::Incoming, Request, Response, Method, StatusCode};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use async_compat::CompatExt;
use smol_timeout2::TimeoutExt;
use http_body_util::BodyExt as _;
use std::convert::Infallible;
use pin_project::pin_project;
use rand::RngCore;
use sillad::{dialer::Dialer, listener::Listener, Pipe};

#[pin_project]
pub struct MeeklikePipe {
    #[pin]
    read: bipe::BipeReader,
    #[pin]
    write: bipe::BipeWriter,
}

impl AsyncRead for MeeklikePipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().read.poll_read(cx, buf)
    }
}

impl AsyncWrite for MeeklikePipe {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().write.poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().write.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().write.poll_close(cx)
    }
}

impl Pipe for MeeklikePipe {
    fn protocol(&self) -> &str { "meeklike" }
    fn remote_addr(&self) -> Option<&str> { None }
}

struct CryptoState {
    aead: ChaCha20Poly1305,
}

impl CryptoState {
    fn new(key: [u8; 32]) -> Self {
        Self { aead: ChaCha20Poly1305::new(&key.into()) }
    }

    fn encrypt(&self, data: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mut out = self.aead.encrypt(&nonce.into(), data).unwrap();
        let mut result = Vec::with_capacity(12 + out.len());
        result.extend_from_slice(&nonce);
        result.append(&mut out);
        result
    }

    fn decrypt(&self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        if data.len() < 12 {
            return Err(Error::new(ErrorKind::InvalidData, "missing nonce"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&data[..12]);
        self.aead
            .decrypt(&nonce.into(), &data[12..])
            .map_err(|_| Error::new(ErrorKind::InvalidData, "decrypt failed"))
    }
}

pub struct MeeklikeDialer<D: Dialer> {
    pub inner: D,
    pub key: [u8; 32],
}

#[async_trait]
impl<D: Dialer> Dialer for MeeklikeDialer<D> {
    type P = MeeklikePipe;
    async fn dial(&self) -> std::io::Result<Self::P> {
        let lower = self.inner.dial().await?;
        let io = TokioIo::new(lower.compat());
        let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, http_body_util::Full<Bytes>>(io)
            .await
            .map_err(Error::other)?;
        smolscale::spawn(conn.compat()).detach();

        let (mut write_in, read_in) = bipe::bipe(32768);
        let (write_out, mut read_out) = bipe::bipe(32768);
        let up = CryptoState::new(derive_key("up", &self.key));
        let down = CryptoState::new(derive_key("dn", &self.key));
        let mut id = [0u8;32];
        rand::thread_rng().fill_bytes(&mut id);
        smolscale::spawn(async move {
            loop {
                let data: Vec<u8> = {
                    let mut buf = vec![0u8; 8192];
                    match read_out
                        .read(&mut buf)
                        .timeout(Duration::from_secs(30))
                        .await
                    {
                        Some(Ok(n)) => {
                            buf.truncate(n);
                            buf
                        }
                        _ => Vec::new(),
                    }
                };
                let mut body = Vec::with_capacity(32 + data.len() + 28);
                body.extend_from_slice(&id);
                body.extend_from_slice(&up.encrypt(&data));
                let req = Request::post("/")
                    .body(http_body_util::Full::new(Bytes::from(body)))
                    .unwrap();
                let resp = match sender.send_request(req).await {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let bytes = match resp.into_body().collect().await {
                    Ok(b) => b.to_bytes(),
                    Err(_) => break,
                };
                let plain = match down.decrypt(&bytes) {
                    Ok(p) => p,
                    Err(_) => break,
                };
                if write_in.write_all(&plain).await.is_err() {
                    break;
                }
            }
        }).detach();
        Ok(MeeklikePipe { read: read_in, write: write_out })
    }
}

pub struct MeeklikeListener<L: Listener> {
    recv: Receiver<MeeklikePipe>,
    _task: async_task::Task<std::io::Result<()>>,
    _phantom: std::marker::PhantomData<L>,
}

impl<L: Listener> MeeklikeListener<L> {
    pub fn new(mut inner: L, key: [u8; 32]) -> Self {
        let (send, recv) = async_channel::unbounded();
        let key = Arc::new(key);
        let task = smolscale::spawn(async move {
            let conns: Arc<DashMap<[u8; 32], Arc<futures_util::lock::Mutex<ServerConn>>>> = Arc::new(DashMap::new());
            loop {
                let lower = inner.accept().await?;
                let conns = conns.clone();
                let send = send.clone();
                let key = key.clone();
                smolscale::spawn(async move {
                    let service = service_fn(move |req| handle_req(req, conns.clone(), send.clone(), key.clone()));
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(lower.compat()), service)
                        .await
                    {
                        tracing::warn!(err = ?e, "meeklike connection error");
                    }
                    Ok::<(), std::io::Error>(())
                }).detach();
            }
        });
        Self { recv, _task: task, _phantom: std::marker::PhantomData }
    }
}

struct ServerConn {
    incoming: bipe::BipeWriter,
    outgoing: bipe::BipeReader,
    up: CryptoState,
    down: CryptoState,
}

async fn handle_req(
    req: Request<Incoming>,
    conns: Arc<DashMap<[u8; 32], Arc<futures_util::lock::Mutex<ServerConn>>>>,
    send: Sender<MeeklikePipe>,
    key: Arc<[u8; 32]>,
) -> Result<Response<http_body_util::Full<Bytes>>, Infallible> {
    if req.method() != Method::POST || req.uri().path() != "/" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(http_body_util::Full::new(Bytes::new()))
            .unwrap());
    }
    let body = req
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_else(|_| Bytes::new());
    if body.len() < 32 + 28 {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(http_body_util::Full::new(Bytes::new()))
            .unwrap());
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&body[..32]);
    let payload = &body[32..];
    let entry = conns.entry(id).or_insert_with(|| {
        let (write_in, read_in) = bipe::bipe(32768);
        let (write_out, read_out) = bipe::bipe(32768);
        let pipe = MeeklikePipe { read: read_in, write: write_out };
        let send2 = send.clone();
        smolscale::spawn(async move { let _ = send2.send(pipe).await; }).detach();
        Arc::new(futures_util::lock::Mutex::new(ServerConn {
            incoming: write_in,
            outgoing: read_out,
            up: CryptoState::new(derive_key("up", &*key)),
            down: CryptoState::new(derive_key("dn", &*key)),
        }))
    });
    let conn = entry.value().clone();
    {
        let mut c = conn.lock().await;
        if let Ok(plain) = c.up.decrypt(payload) {
            let _ = c.incoming.write_all(&plain).await;
        }
    }
    let data = {
        let mut c = conn.lock().await;
        let result = {
            let mut buf = vec![0u8; 8192];
            match c
                .outgoing
                .read(&mut buf)
                .timeout(Duration::from_secs(30))
                .await
            {
                Some(Ok(n)) => {
                    buf.truncate(n);
                    c.down.encrypt(&buf)
                }
                _ => c.down.encrypt(&[]),
            }
        };
        result
    };
    Ok(Response::new(http_body_util::Full::new(Bytes::from(data))))
}

#[async_trait]
impl<L: Listener> Listener for MeeklikeListener<L> {
    type P = MeeklikePipe;
    async fn accept(&mut self) -> std::io::Result<Self::P> {
        self.recv.recv().await.map_err(|_| Error::new(ErrorKind::BrokenPipe, "closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sillad::tcp::{TcpListener, TcpDialer};
    use futures_util::{AsyncReadExt, AsyncWriteExt};
    use std::net::{SocketAddr, IpAddr, Ipv4Addr};

    #[test]
    fn ping_pong() {
        smolscale::block_on(async {
            let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await.unwrap();
            let addr = listener.local_addr().await;
            let key = [7u8; 32];
            let mut meek_listener = MeeklikeListener::new(listener, key);

            let dialer = MeeklikeDialer { inner: TcpDialer { dest_addr: addr }, key };

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
