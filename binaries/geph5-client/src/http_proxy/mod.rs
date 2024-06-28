mod address;
mod http_client;
mod rt_compat;

use std::{net::SocketAddr, str::FromStr as _};

pub async fn run_http_proxy(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    let shared_server: SharedProxyServer = ProxyServer::new_shared(ctx.clone());
    let listen = ctx.init().http_proxy_listen;
    if let Some(listen) = listen {
        let tcp_listener = tokio::net::TcpListener::bind(&listen).await?;
        let mut join_set = JoinSet::new();
        loop {
            let (stream, addr) = match tcp_listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::info!(%e, "failed to accept inbound HTTP proxy connection");
                    continue;
                }
            };
            let ctx = ctx.clone();
            let cloned_server = shared_server.clone();
            join_set.spawn(async move {
                tracing::trace!(%addr, "accepted a HTTP proxy connection");

                let service = service_fn(move |req: Request<Incoming>| {
                    server_dispatch(req, addr, cloned_server.clone(), ctx.clone())
                });

                let result = hyper::server::conn::http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
                if let Err(e) = result {
                    tracing::error!(%addr, %e, "error serving HTTP proxy conn: {addr}");
                }
            });
        }
        // while let Some(_) = join_set.join_next().await {}
        // Ok(())
    } else {
        smol::future::pending().await
    }
}
type SharedProxyServer = std::sync::Arc<ProxyServer>;

async fn server_dispatch(
    mut req: Request<Incoming>,
    client_addr: SocketAddr,
    proxy_server: SharedProxyServer,
    ctx: AnyCtx<Config>,
) -> std::io::Result<Response<HttpEither<BoxBody<Bytes, hyper::Error>, Empty<Bytes>>>> {
    let host = match host_addr(req.uri()) {
        None => {
            if req.uri().authority().is_some() {
                tracing::trace!(
                    method = %req.method(),
                    uri = %req.uri(),
                    "HTTP URI doesn't have a valid host"
                );
                return Ok(make_bad_request());
            } else {
                tracing::trace!(
                    method = %req.method(),
                    uri = %req.uri(),
                    "HTTP URI doesn't have a valid host"
                );
            }
            match req.headers().get("Host") {
                None => {
                    return Ok(make_bad_request());
                }
                Some(hhost) => match hhost.to_str() {
                    Err(..) => {
                        return Ok(make_bad_request());
                    }
                    Ok(shost) => {
                        match Authority::from_str(shost) {
                            Ok(authority) => {
                                match authority_addr(req.uri().scheme_str(), &authority) {
                                    Some(host) => {
                                        tracing::trace!(
                                            method = %req.method(),
                                            uri = %req.uri(),
                                            host = %host,
                                            "HTTP URI got host from header"
                                        );

                                        // Reassemble URI
                                        let mut parts = req.uri().clone().into_parts();
                                        if parts.scheme.is_none() {
                                            // Use http as default.
                                            parts.scheme = Some(Scheme::HTTP);
                                        }
                                        parts.authority = Some(authority);

                                        // Replaces URI
                                        *req.uri_mut() =
                                            Uri::from_parts(parts).expect("Reassemble URI failed");

                                        tracing::trace!(
                                            method = %req.method(),
                                            uri = %req.uri(),
                                            "Reassembled URI from \"Host\""
                                        );

                                        host
                                    }
                                    None => {
                                        tracing::trace!(
                                            method = %req.method(),
                                            uri = %req.uri(),
                                            host_header_value = %shost,
                                            "HTTP \"Host\" header invalid"
                                        );

                                        return Ok(make_bad_request());
                                    }
                                }
                            }
                            Err(..) => {
                                tracing::trace!(
                                    method = %req.method(),
                                    uri = %req.uri(),
                                    host_header_value = ?hhost,
                                    "HTTP \"Host\" header is not an Authority"
                                );

                                return Ok(make_bad_request());
                            }
                        }
                    }
                },
            }
        }
        Some(h) => h,
    };
    if Method::CONNECT == req.method() {
        tracing::trace!(
            method = %req.method(),
            client_addr = %client_addr,
            host = %host,
            "CONNECT relay connected"
        );
        tokio::spawn(async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    tracing::trace!(
                        client_addr = %client_addr,
                        host = %host,
                        "CONNECT tunnel upgrade success"
                    );
                    let stream = open_conn(&ctx, "tcp", &host.to_string()).await;
                    if let Ok(stream) = stream {
                        establish_connect_tunnel(upgraded, stream, client_addr).await
                    }
                }
                Err(e) => {
                    tracing::info!(
                        client_addr = %client_addr,
                        host = %host,
                        error = %e,
                        "Failed to upgrade TCP tunnel"
                    );
                }
            }
        });
        Ok(Response::new(HttpEither::Right(Empty::new())))
    } else {
        let method = req.method().clone();
        tracing::trace!(method = %method, host = %host, "HTTP request received");
        let conn_keep_alive = check_keep_alive(req.version(), req.headers(), true);
        clear_hop_headers(req.headers_mut());
        set_conn_keep_alive(req.version(), req.headers_mut(), conn_keep_alive);
        let (parts, body) = req.into_parts();
        let body = match body.collect().await {
            Ok(c) => c.boxed(),
            Err(_) => return Ok(make_bad_request()),
        };
        let mut res: Response<HttpEither<BoxBody<Bytes, hyper::Error>, Empty<Bytes>>> =
            match proxy_server
                .client
                .request(Request::from_parts(parts, body))
                .await
            {
                Ok(res) => res.map(|b| HttpEither::Left(b.boxed())),
                Err(err) => {
                    tracing::trace!(
                        method = %method,
                        client_addr = %client_addr,
                        proxy_addr = "127.0.0.1:1080",
                        host = %host,
                        error = %err,
                        "HTTP relay failed"
                    );
                    let mut resp = Response::new(HttpEither::Left(
                        Full::new(Bytes::from(format!("Relay failed to {}", host)))
                            .map_err(|_| unreachable!())
                            .boxed(),
                    ));
                    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    return Ok(resp);
                }
            };
        let res_keep_alive =
            conn_keep_alive && check_keep_alive(res.version(), res.headers(), false);
        clear_hop_headers(res.headers_mut());
        set_conn_keep_alive(res.version(), res.headers_mut(), res_keep_alive);
        Ok(res)
    }
}
use anyctx::AnyCtx;
use async_compat::CompatExt;
use bytes::Bytes;
use futures_util::{
    future::{self, Either},
    FutureExt,
};
use http::{
    uri::{Authority, Scheme},
    HeaderMap, HeaderValue, Method, Uri, Version,
};
use http_body_util::{combinators::BoxBody, BodyExt, Either as HttpEither, Empty, Full};
use hyper::{
    body::Incoming, service::service_fn, upgrade::Upgraded, Request, Response, StatusCode,
};
use tokio::task::JoinSet;

async fn establish_connect_tunnel(
    upgraded: Upgraded,
    stream: picomux::Stream,
    client_addr: SocketAddr,
) {
    use tokio::io::{copy, split};

    let (mut r, mut w) = split(rt_compat::HyperRtCompat::new(upgraded));
    let (mut svr_r, mut svr_w) = split(stream.compat());

    let rhalf = copy(&mut r, &mut svr_w);
    let whalf = copy(&mut svr_r, &mut w);

    tracing::trace!(
        client_addr = %client_addr,
        "CONNECT relay established"
    );

    match future::select(rhalf.boxed(), whalf.boxed()).await {
        Either::Left((Ok(..), _)) => tracing::trace!(
            client_addr = %client_addr,
            "CONNECT relay closed"
        ),
        Either::Left((Err(err), _)) => {
            tracing::trace!(
                client_addr = %client_addr,
                error = %err,
                "CONNECT relay closed with error"
            );
        }
        Either::Right((Ok(..), _)) => tracing::trace!(
            client_addr = %client_addr,
            "CONNECT relay closed"
        ),
        Either::Right((Err(err), _)) => {
            tracing::trace!(
                client_addr = %client_addr,
                error = %err,
                "CONNECT relay closed with error"
            );
        }
    }

    tracing::trace!(
        client_addr = %client_addr,
        "CONNECT relay closed"
    );
}

fn make_bad_request() -> Response<HttpEither<BoxBody<Bytes, hyper::Error>, Empty<Bytes>>> {
    let mut resp: Response<HttpEither<BoxBody<Bytes, hyper::Error>, Empty<Bytes>>> = Response::new(
        HttpEither::Left(Empty::new().map_err(|_| unreachable!()).boxed()),
    );
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::{client_inner::open_conn, Config};

use self::address::{host_addr, Address};
fn authority_addr(scheme_str: Option<&str>, authority: &Authority) -> Option<Address> {
    // RFC7230 indicates that we should ignore userinfo
    // https://tools.ietf.org/html/rfc7230#section-5.3.3

    // Check if URI has port
    let port = match authority.port_u16() {
        Some(port) => port,
        None => {
            match scheme_str {
                None => 80, // Assume it is http
                Some("http") => 80,
                Some("https") => 443,
                _ => return None, // Not supported
            }
        }
    };

    let host_str = authority.host();

    // RFC3986 indicates that IPv6 address should be wrapped in [ and ]
    // https://tools.ietf.org/html/rfc3986#section-3.2.2
    //
    // Example: [::1] without port
    if host_str.starts_with('[') && host_str.ends_with(']') {
        // Must be a IPv6 address
        let addr = &host_str[1..host_str.len() - 1];
        match addr.parse::<Ipv6Addr>() {
            Ok(a) => Some(Address::from(SocketAddr::new(IpAddr::V6(a), port))),
            // Ignore invalid IPv6 address
            Err(..) => None,
        }
    } else {
        // It must be a IPv4 address
        match host_str.parse::<Ipv4Addr>() {
            Ok(a) => Some(Address::from(SocketAddr::new(IpAddr::V4(a), port))),
            // Should be a domain name, or a invalid IP address.
            // Let DNS deal with it.
            Err(..) => Some(Address::DomainNameAddress(host_str.to_owned(), port)),
        }
    }
}

fn check_keep_alive(version: Version, headers: &HeaderMap<HeaderValue>, check_proxy: bool) -> bool {
    let mut conn_keep_alive = match version {
        Version::HTTP_10 => false,
        Version::HTTP_11 => true,
        _ => unimplemented!("HTTP Proxy only supports 1.0 and 1.1"),
    };

    if check_proxy {
        // Modern browers will send Proxy-Connection instead of Connection
        // for HTTP/1.0 proxies which blindly forward Connection to remote
        //
        // https://tools.ietf.org/html/rfc7230#appendix-A.1.2
        for value in headers.get_all("Proxy-Connection") {
            if let Ok(value) = value.to_str() {
                if value.eq_ignore_ascii_case("close") {
                    conn_keep_alive = false;
                } else {
                    for part in value.split(',') {
                        let part = part.trim();
                        if part.eq_ignore_ascii_case("keep-alive") {
                            conn_keep_alive = true;
                            break;
                        }
                    }
                }
            }
        }
    }

    // Connection will replace Proxy-Connection
    //
    // But why client sent both Connection and Proxy-Connection? That's not standard!
    for value in headers.get_all("Connection") {
        if let Ok(value) = value.to_str() {
            if value.eq_ignore_ascii_case("close") {
                conn_keep_alive = false;
            } else {
                for part in value.split(',') {
                    let part = part.trim();

                    if part.eq_ignore_ascii_case("keep-alive") {
                        conn_keep_alive = true;
                        break;
                    }
                }
            }
        }
    }

    conn_keep_alive
}

fn clear_hop_headers(headers: &mut HeaderMap<HeaderValue>) {
    // Clear headers indicated by Connection and Proxy-Connection
    let mut extra_headers = Vec::new();

    for connection in headers.get_all("Connection") {
        if let Ok(conn) = connection.to_str() {
            if !conn.eq_ignore_ascii_case("close") {
                for header in conn.split(',') {
                    let header = header.trim();

                    if !header.eq_ignore_ascii_case("keep-alive") {
                        extra_headers.push(header.to_owned());
                    }
                }
            }
        }
    }

    for connection in headers.get_all("Proxy-Connection") {
        if let Ok(conn) = connection.to_str() {
            if !conn.eq_ignore_ascii_case("close") {
                for header in conn.split(',') {
                    let header = header.trim();

                    if !header.eq_ignore_ascii_case("keep-alive") {
                        extra_headers.push(header.to_owned());
                    }
                }
            }
        }
    }

    for header in extra_headers {
        while headers.remove(&header).is_some() {}
    }

    // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Connection
    const HOP_BY_HOP_HEADERS: [&str; 9] = [
        "Keep-Alive",
        "Transfer-Encoding",
        "TE",
        "Connection",
        "Trailer",
        "Upgrade",
        "Proxy-Authorization",
        "Proxy-Authenticate",
        "Proxy-Connection", // Not standard, but many implementations do send this header
    ];

    for header in &HOP_BY_HOP_HEADERS {
        while headers.remove(*header).is_some() {}
    }
}

fn set_conn_keep_alive(version: Version, headers: &mut HeaderMap<HeaderValue>, keep_alive: bool) {
    match version {
        Version::HTTP_10 => {
            // HTTP/1.0 close connection by default
            if keep_alive {
                headers.insert("Connection", HeaderValue::from_static("keep-alive"));
            }
        }
        Version::HTTP_11 => {
            // HTTP/1.1 keep-alive connection by default
            if !keep_alive {
                headers.insert("Connection", HeaderValue::from_static("close"));
            }
        }
        _ => unimplemented!("HTTP Proxy only supports 1.0 and 1.1"),
    }
}

#[derive(Clone)]
pub struct ProxyServer {
    client: http_client::CtxClient,
}

impl ProxyServer {
    fn new(ctx: AnyCtx<Config>) -> ProxyServer {
        let connector = http_client::Connector::new(ctx);
        let proxy_client: http_client::CtxClient =
            hyper_util::client::legacy::Builder::new(hyper_util::rt::TokioExecutor::new())
                .http1_preserve_header_case(true)
                .http1_title_case_headers(true)
                .build(connector);
        ProxyServer {
            client: proxy_client,
        }
    }
    fn new_shared(ctx: AnyCtx<Config>) -> SharedProxyServer {
        std::sync::Arc::new(ProxyServer::new(ctx))
    }
}
