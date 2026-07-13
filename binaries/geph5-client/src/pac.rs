use anyctx::AnyCtx;
use bytes::Bytes;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;

use crate::Config;

pub async fn pac_serve(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(if let Some(listen) = ctx.init().pac_listen {
        listen
    } else {
        std::future::pending().await
    })
    .await?;

    // Clone the context for use in the spawned tasks
    let ctx = ctx.clone();

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        // Clone the context for this connection
        let ctx = ctx.clone();

        tokio::task::spawn(async move {
            // Create a service function that captures a clone of `ctx`
            let service = service_fn(move |req| serve_pac(req, ctx.clone()));
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn serve_pac(
    _req: Request<hyper::body::Incoming>,
    ctx: AnyCtx<Config>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // A wildcard bind address is not dialable; the PAC is served to local
    // browsers, so point them at loopback.
    let to_loopback = |mut addr: std::net::SocketAddr| {
        if addr.ip().is_unspecified() {
            addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
        }
        addr
    };
    // pac_listen can be configured independently of http_proxy_listen; do not
    // assume an HTTP proxy exists (unwrapping it panicked the request task).
    // Prefer HTTP, fall back to SOCKS, and otherwise tell the browser to go
    // direct rather than crash.
    let init = ctx.init();
    let directive = if let Some(http) = init.http_proxy_listen {
        format!("PROXY {}", to_loopback(http))
    } else if let Some(socks) = init.socks5_listen {
        let socks = to_loopback(socks);
        format!("SOCKS5 {socks}; SOCKS {socks}")
    } else {
        "DIRECT".to_string()
    };
    Ok(Response::new(Full::new(Bytes::from(format!(
        "function FindProxyForURL(url, host){{return '{directive}';}}"
    )))))
}
