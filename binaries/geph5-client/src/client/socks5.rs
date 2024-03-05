use std::net::Ipv4Addr;

use anyctx::AnyCtx;
use futures_util::AsyncReadExt as _;
use sillad::listener::Listener as _;
use smol::future::FutureExt as _;
use socksv5::v5::{
    read_handshake, read_request, write_auth_method, write_request_status, SocksV5AuthMethod,
    SocksV5Host, SocksV5RequestStatus,
};

use crate::client::inner::open_conn;

use super::Config;

pub async fn socks5_loop(ctx: AnyCtx<Config>) -> anyhow::Result<()> {
    let mut listener = sillad::tcp::TcpListener::bind(ctx.init().socks5_listen).await?;
    loop {
        let client = listener.accept().await?;
        let ctx = ctx.clone();
        smolscale::spawn(async move {
            tracing::debug!("socks5 connection accepted");
            let (mut read_client, mut write_client) = client.split();
            let _handshake = read_handshake(&mut read_client).await?;
            write_auth_method(&mut write_client, SocksV5AuthMethod::Noauth).await?;
            let request = read_request(&mut read_client).await?;
            let port = request.port;
            let domain: String = match &request.host {
                SocksV5Host::Domain(dom) => String::from_utf8_lossy(dom).parse()?,
                SocksV5Host::Ipv4(v4) => {
                    let v4addr = Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3]);
                    v4addr.to_string()
                }
                _ => anyhow::bail!("IPv6 not supported"),
            };
            let remote_addr = format!("{domain}:{port}");
            tracing::debug!(
                remote_addr = display(&remote_addr),
                "socks5 request received"
            );
            let stream = open_conn(&ctx, &remote_addr).await?;
            write_request_status(
                &mut write_client,
                SocksV5RequestStatus::Success,
                request.host,
                port,
            )
            .await?;
            tracing::debug!(remote_addr = display(&remote_addr), "connection opened");
            let (read_stream, write_stream) = stream.split();
            smol::io::copy(read_stream, write_client)
                .race(smol::io::copy(read_client, write_stream))
                .await?;
            anyhow::Ok(())
        })
        .detach();
    }
}
