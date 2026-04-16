use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Uri, client::conn::http1};
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};

use crate::{
    Config,
    tunneled_http::{HyperRtCompat, TunneledConnection},
};

pub struct TunneledHttpTransport {
    ctx: anyctx::AnyCtx<Config>,
    uri: Uri,
    authority: String,
    tls_host: Option<String>,
}

impl TunneledHttpTransport {
    pub fn new(ctx: anyctx::AnyCtx<Config>, url: String) -> Self {
        let uri: Uri = url
            .parse()
            .expect("tunneled broker URL must be a valid URI");
        let authority = uri
            .authority()
            .expect("tunneled broker URL must include authority")
            .to_string();
        let tls_host = (uri.scheme_str() == Some("https")).then(|| {
            uri.host()
                .expect("https tunneled broker URL must include host")
                .to_string()
        });
        Self {
            ctx,
            uri,
            authority,
            tls_host,
        }
    }
}

#[async_trait]
impl RpcTransport for TunneledHttpTransport {
    type Error = anyhow::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        tracing::debug!(
            method = req.method,
            tunneled_broker = %self.uri,
            "calling broker through Geph"
        );
        let host = self
            .uri
            .host()
            .context("tunneled broker URI missing host")?;
        let port = self
            .uri
            .port_u16()
            .unwrap_or(if self.tls_host.is_some() { 443 } else { 80 });
        let remote = format!("{host}:{port}");
        let conn = crate::session::open_conn(&self.ctx, "tcp", &remote).await?;
        let io = if let Some(tls_host) = &self.tls_host {
            let tls = async_native_tls::TlsConnector::new()
                .connect(tls_host, conn)
                .await
                .context("cannot establish tunneled TLS session")?;
            Io::Tls(HyperRtCompat::new(async_compat::Compat::new(tls)))
        } else {
            Io::Plain(HyperRtCompat::new(TunneledConnection::new(conn)))
        };

        let (mut sender, connection) = http1::handshake(io).await?;
        smolscale::spawn(async move {
            if let Err(err) = connection.await {
                tracing::debug!(err = debug(err), "tunneled broker HTTP connection ended");
            }
        })
        .detach();

        let body = Full::new(Bytes::from(serde_json::to_vec(&req)?));
        let request = Request::builder()
            .method(Method::POST)
            .uri(
                self.uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or("/"),
            )
            .header("host", &self.authority)
            .header("content-type", "application/json")
            .body(body)
            .context("could not build tunneled broker request")?;
        let response = sender
            .send_request(request)
            .await
            .context("cannot send tunneled broker request")?;
        let body = response
            .into_body()
            .collect()
            .await
            .context("cannot read tunneled broker response")?
            .to_bytes();
        Ok(serde_json::from_slice(&body)?)
    }
}

enum Io {
    Plain(HyperRtCompat<TunneledConnection>),
    Tls(HyperRtCompat<async_compat::Compat<async_native_tls::TlsStream<Box<dyn sillad::Pipe>>>>),
}

impl hyper::rt::Read for Io {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.as_mut().get_mut() {
            Io::Plain(inner) => std::pin::Pin::new(inner).poll_read(cx, buf),
            Io::Tls(inner) => std::pin::Pin::new(inner).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Io {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match self.as_mut().get_mut() {
            Io::Plain(inner) => std::pin::Pin::new(inner).poll_write(cx, buf),
            Io::Tls(inner) => std::pin::Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.as_mut().get_mut() {
            Io::Plain(inner) => std::pin::Pin::new(inner).poll_flush(cx),
            Io::Tls(inner) => std::pin::Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.as_mut().get_mut() {
            Io::Plain(inner) => std::pin::Pin::new(inner).poll_shutdown(cx),
            Io::Tls(inner) => std::pin::Pin::new(inner).poll_shutdown(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Io::Plain(inner) => inner.is_write_vectored(),
            Io::Tls(inner) => inner.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match self.as_mut().get_mut() {
            Io::Plain(inner) => std::pin::Pin::new(inner).poll_write_vectored(cx, bufs),
            Io::Tls(inner) => std::pin::Pin::new(inner).poll_write_vectored(cx, bufs),
        }
    }
}
