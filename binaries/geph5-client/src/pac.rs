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
    let pac_addr = ctx.init().http_proxy_listen.unwrap();
    Ok(Response::new(Full::new(Bytes::from(format!(
        "function FindProxyForURL(url, host){{return 'PROXY {}';}}",
        pac_addr
    )))))
}
