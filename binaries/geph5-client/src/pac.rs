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
        smol::future::pending().await
    })
    .await?;

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service_fn(serve_pac))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn serve_pac(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from(
        "function FindProxyForURL(url, host){return 'PROXY 127.0.0.1:9910';}",
    ))))
}
