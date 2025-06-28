use std::{
    sync::{atomic::AtomicUsize, Arc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_channel::{Receiver, Sender};
use async_compat::CompatExt;
use async_task::Task;
use base64::{prelude::BASE64_STANDARD, Engine};
use bytes::Bytes;
use dashmap::DashMap;
use futures_lite::FutureExt;
use http_body_util::BodyExt;
use hyper::{body::Incoming, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rand::Rng;
use sillad::{dialer::Dialer, listener::Listener};
use smol_timeout2::TimeoutExt;
use stdcode::StdcodeSerializeExt;

use crate::{
    crypto::{Datagram, PresharedSecret},
    MeeklikeConfig,
};

/// An unreliable datagram connection.
pub struct DgConnection {
    send: Sender<Bytes>,
    recv: Receiver<Bytes>,

    autoclean: Option<Box<dyn FnOnce() + Send + Sync + 'static>>,
}

impl Drop for DgConnection {
    fn drop(&mut self) {
        if let Some(f) = self.autoclean.take() {
            f()
        }
    }
}

impl DgConnection {
    pub fn new<D: Dialer>(
        cfg: MeeklikeConfig,
        secret: Arc<PresharedSecret>,
        stream_id: u128,
        inner: Arc<D>,
    ) -> Self {
        let (send_up, recv_up) = async_channel::bounded(1000);
        let (send_dn, recv_dn) = async_channel::bounded(1000);
        for _ in 0..cfg.max_inflight {
            smolscale::spawn(dg_client_backhaul(
                secret.clone(),
                stream_id,
                cfg,
                inner.clone(),
                recv_up.clone(),
                send_dn.clone(),
            ))
            .detach();
        }
        Self {
            send: send_up,
            recv: recv_dn,
            autoclean: None,
        }
    }

    pub fn send(&self, bts: Bytes) {
        let _ = self.send.try_send(bts);
    }

    pub async fn recv(&self) -> anyhow::Result<Bytes> {
        Ok(self.recv.recv().await?)
    }
}

#[tracing::instrument(skip_all, fields(stream_id))]
async fn dg_client_backhaul<D: Dialer>(
    secret: Arc<PresharedSecret>,
    stream_id: u128,
    cfg: MeeklikeConfig,

    inner: Arc<D>,
    recv_up: Receiver<Bytes>,
    send_dn: Sender<Bytes>,
) -> anyhow::Result<()> {
    tracing::debug!("backhaul started");
    static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
    let mut interval = 0.02;
    loop {
        let to_send = match recv_up
            .recv()
            .timeout(Duration::from_secs_f64(interval))
            .await
        {
            None => Bytes::new(),
            Some(x) => x?,
        };
        let to_send = secret.encrypt_up(
            &Datagram {
                ts: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                stream_id,
                inner: to_send,
            }
            .stdcode(),
        );
        tracing::debug!(to_send = to_send.len(), "going to send");
        let to_send = if cfg.base64 {
            BASE64_STANDARD.encode(to_send).as_bytes().to_vec()
        } else {
            to_send
        };
        let res = async {
            let inflight = INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::debug!(inflight, "inflight incr");
            scopeguard::defer!({
                let inflight = INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) - 1;
                tracing::debug!(inflight, "inflight decr");
            });
            let lower = inner.dial().await?;
            let io = TokioIo::new(lower.compat());
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake::<_, http_body_util::Full<Bytes>>(io)
                    .await
                    .map_err(std::io::Error::other)?;

            async {
                conn.await?;
                anyhow::Ok(())
            }
            .race(async {
                let up_len = to_send.len();
                let start = Instant::now();
                let req = Request::post("/")
                    .body(http_body_util::Full::new(Bytes::from(to_send)))
                    .unwrap();
                let resp = sender.send_request(req).await?;
                let bytes = resp.into_body().collect().await?.to_bytes();
                let bytes = if cfg.base64 {
                    Bytes::from(BASE64_STANDARD.decode(bytes)?)
                } else {
                    bytes
                };
                let elapsed = start.elapsed();
                tracing::debug!(
                    up_len,
                    dn_len = bytes.len(),
                    elapsed = debug(elapsed),
                    "response gotten"
                );
                let plain = secret.decrypt_dn(&bytes)?;
                let plain: Datagram = stdcode::deserialize(&plain)?;
                check_timestamp(&plain)?;
                if plain.stream_id != stream_id {
                    anyhow::bail!("got a response with the wrong stream ID")
                }
                if !plain.inner.is_empty() {
                    send_dn.try_send(plain.inner)?;
                    interval = 0.1;
                } else {
                    tracing::debug!(interval, "empty, so increasing interval");
                    interval = rand::thread_rng().gen_range(interval..interval * 2.0);
                    interval = interval.min(30.0);
                }
                anyhow::Ok(())
            })
            .await
        };
        match res.timeout(Duration::from_secs(10)).await {
            None => tracing::warn!("req/resp timed out"),
            Some(Err(err)) => tracing::warn!(err = debug(err), "req/resp died"),
            _ => {}
        }
    }
}

/// A listener of unreliable datagram connections.
pub struct DgListener {
    recv_accepted: Receiver<DgConnection>,
    _task: Task<anyhow::Result<()>>,
}

impl DgListener {
    pub fn new<L: Listener>(
        cfg: MeeklikeConfig,
        secret: Arc<PresharedSecret>,
        mut inner: L,
    ) -> Self {
        let (send_accepted, recv_accepted) = async_channel::unbounded();
        let _task = smolscale::spawn(async move {
            let conns = Arc::new(DashMap::new());
            loop {
                let lower = inner.accept().await?;
                let send = send_accepted.clone();
                let secret = secret.clone();
                let conns = conns.clone();
                smolscale::spawn(async move {
                    let service = service_fn(move |req| {
                        handle_req(cfg, req, conns.clone(), send.clone(), secret.clone())
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(lower.compat()), service)
                        .await
                    {
                        tracing::warn!(err = ?e, "meeklike connection error");
                    }
                    Ok::<(), std::io::Error>(())
                })
                .detach();
            }
        });
        Self {
            recv_accepted,
            _task,
        }
    }

    pub async fn accept(&self) -> anyhow::Result<DgConnection> {
        Ok(self.recv_accepted.recv().await?)
    }
}

async fn handle_req(
    cfg: MeeklikeConfig,
    req: Request<Incoming>,
    conns: Arc<DashMap<u128, (Sender<Bytes>, Receiver<Bytes>)>>,
    send: Sender<DgConnection>,
    secret: Arc<PresharedSecret>,
) -> Result<Response<http_body_util::Full<Bytes>>, anyhow::Error> {
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
    let body = if cfg.base64 {
        Bytes::from(BASE64_STANDARD.decode(&body)?)
    } else {
        body
    };
    let dg: Datagram = stdcode::deserialize(&secret.decrypt_up(&body)?)?;
    check_timestamp(&dg)?;
    let conns2 = conns.clone();
    let stream_id = dg.stream_id;
    let (send_up, recv_dn) = conns
        .entry(dg.stream_id)
        .or_insert_with(|| {
            let (send_up, recv_up) = async_channel::bounded(1000);
            let (send_dn, recv_dn) = async_channel::bounded(1000);
            let conn = DgConnection {
                send: send_dn,
                recv: recv_up,
                autoclean: Some(Box::new(move || {
                    conns2.remove(&stream_id);
                })),
            };
            let _ = send.try_send(conn);
            (send_up, recv_dn)
        })
        .value()
        .clone();
    if !dg.inner.is_empty() {
        let _ = send_up.try_send(dg.inner);
    }

    let resp = if let Some(Ok(val)) = recv_dn.recv().timeout(Duration::from_millis(150)).await {
        val
    } else {
        Bytes::new()
    };

    let resp = Datagram {
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        stream_id,
        inner: resp,
    };

    let data = secret.encrypt_dn(&resp.stdcode());
    let data = if cfg.base64 {
        BASE64_STANDARD.encode(&data).as_bytes().to_vec()
    } else {
        data
    };
    let mut response = Response::new(http_body_util::Full::new(Bytes::from(data)));
    response
        .headers_mut()
        .insert("content-type", "application/octet-stream".parse().unwrap());
    Ok(response)
}

fn check_timestamp(dg: &Datagram) -> anyhow::Result<()> {
    // TODO check
    Ok(())
}
