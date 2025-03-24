use crate::{client_inner::open_conn, litecopy::litecopy, taskpool::add_task};

use anyctx::AnyCtx;

use futures_util::AsyncReadExt as _;
use nursery_macro::nursery;
use sillad::listener::Listener as _;
use smol::future::FutureExt as _;
use socksv5::v5::{
    read_handshake, read_request, write_auth_method, write_request_status, SocksV5AuthMethod,
    SocksV5Host, SocksV5RequestStatus,
};
use std::net::Ipv4Addr;

use super::Config;

#[tracing::instrument(skip_all)]
pub async fn socks5_loop(ctx: &AnyCtx<Config>) -> anyhow::Result<()> {
    if let Some(listen_addr) = ctx.init().socks5_listen {
        let mut listener = sillad::tcp::TcpListener::bind(listen_addr).await?;
        nursery!({
            loop {
                let client = listener.accept().await?;
                let task = spawn!(async {
                    tracing::trace!("socks5 connection accepted");
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
                    tracing::trace!(
                        remote_addr = display(&remote_addr),
                        "socks5 request received"
                    );
                    let stream = open_conn(ctx, "tcp", &remote_addr).await?;
                    write_request_status(
                        &mut write_client,
                        SocksV5RequestStatus::Success,
                        request.host,
                        port,
                    )
                    .await?;
                    tracing::trace!(remote_addr = display(&remote_addr), "connection opened");
                    let (read_stream, write_stream) = stream.split();
                    litecopy(read_stream, write_client)
                        .race(litecopy(read_client, write_stream))
                        .await?;
                    anyhow::Ok(())
                });
                if let Some(task_limit) = ctx.init().task_limit {
                    add_task(task_limit, task);
                } else {
                    task.detach();
                }
            }
        })
    } else {
        smol::future::pending().await
    }
}
