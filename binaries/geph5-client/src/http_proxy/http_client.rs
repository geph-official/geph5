use anyctx::AnyCtx;
use async_compat::{Compat, CompatExt};
use bytes::Bytes;
use futures_util::{future::BoxFuture, FutureExt};
use http_body_util::combinators::BoxBody;
use hyper::Uri;
use hyper_util::client::legacy::connect::Connection;
use pin_project::pin_project;
use std::convert::Infallible;
use std::future::Future;

use std::pin::Pin;
use std::task::{self, Poll};

use crate::{client_inner::open_conn, Config};

use super::address::host_addr;
use super::rt_compat::HyperRtCompat;

#[derive(Clone)]
pub struct Connector {
    ctx: AnyCtx<Config>,
}

impl Connector {
    pub fn new(ctx: AnyCtx<Config>) -> Connector {
        Connector { ctx }
    }
}

impl tower_service::Service<Uri> for Connector {
    type Error = std::io::Error;
    type Future = SocksConnecting;
    type Response = HyperRtCompat<PicomuxConnection>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let ctx = self.ctx.clone();
        SocksConnecting {
            fut: async move {
                match host_addr(&dst) {
                    None => {
                        use std::io::{Error, ErrorKind};
                        let err = Error::new(ErrorKind::Other, "URI must be a valid Address");
                        Err(err)
                    }
                    Some(addr) => open_conn(&ctx, &addr.to_string())
                        .await
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))
                        .map(|c| HyperRtCompat::new(PicomuxConnection(c.compat()))),
                }
            }
            .boxed(),
        }
    }
}
#[pin_project]
pub struct SocksConnecting {
    #[pin]
    fut: BoxFuture<'static, std::io::Result<HyperRtCompat<PicomuxConnection>>>,
}

impl Future for SocksConnecting {
    type Output = std::io::Result<HyperRtCompat<PicomuxConnection>>;
    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}
pub type CtxClient = hyper_util::client::legacy::Client<Connector, BoxBody<Bytes, Infallible>>;

pub struct PicomuxConnection(Compat<picomux::Stream>);

impl Connection for PicomuxConnection {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}

impl tokio::io::AsyncRead for PicomuxConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PicomuxConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
