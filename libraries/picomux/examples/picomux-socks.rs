use clap::{Parser, Subcommand};
use futures_concurrency::future::Race;
use futures_util::TryFutureExt;
use picomux::PicoMux;

use sillad::{
    dialer::Dialer,
    listener::Listener,
    tcp::{TcpDialer, TcpListener},
};
use sillad_sosistab3::{Cookie, dialer::SosistabDialer, listener::SosistabListener};
use socksv5::v5::{
    SocksV5AuthMethod, SocksV5Host, SocksV5RequestStatus, read_handshake, read_request,
    write_auth_method, write_request_status,
};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// SOCKS5 Program
#[derive(Parser)]
struct Socks5 {
    #[command(subcommand)]
    nested: SubCommands,
}

#[derive(Subcommand)]
enum SubCommands {
    Server(ServerCommand),
    Client(ClientCommand),
}

/// Server subcommand
/// Listens on the specified SocketAddr.
#[derive(Parser)]
struct ServerCommand {
    /// socket address to bind the server (e.g., "127.0.0.1:1080")
    #[arg(short, long)]
    listen: SocketAddr,
}

/// Client subcommand
/// Connects to a specified SOCKS5 server.
#[derive(Parser)]
struct ClientCommand {
    /// socket address to bind the socks5 server (e.g., "127.0.0.1:1080")
    #[arg(short, long)]
    listen: SocketAddr,
    /// server address and port to connect to (e.g., "127.0.0.1:1080")
    #[arg(short, long)]
    connect: SocketAddr,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    geph5_rt::block_on(async {
        let socks5 = Socks5::parse();
        match socks5.nested {
            SubCommands::Server(server) => {
                eprintln!("Starting server on {}", server.listen);
                let listener = TcpListener::bind(server.listen).await?;
                let mut listener = SosistabListener::new(listener, Cookie::new("hello"));
                loop {
                    let tcp_stream = listener.accept().await?;
                    let (read, write) = tokio::io::split(tcp_stream);
                    let mux = PicoMux::new(read, write);
                    geph5_rt::spawn(async move {
                        loop {
                            let client = mux.accept().await?;
                            geph5_rt::spawn(
                                handle_server(client)
                                    .map_err(|e| eprintln!("server handling dying: {:?}", e)),
                            )
                            .detach();
                        }
                        #[allow(unreachable_code)]
                        anyhow::Ok(())
                    })
                    .detach()
                }
            }
            SubCommands::Client(client) => {
                eprintln!(
                    "Connecting to server at {}, socks5 at {}",
                    client.connect, client.listen
                );
                let mut listener = TcpListener::bind(client.listen).await?;
                let dialer = TcpDialer {
                    dest_addr: client.connect,
                };
                let dialer = SosistabDialer {
                    inner: dialer,
                    cookie: Cookie::new("hello"),
                };
                loop {
                    let remote_conn = tokio::io::split(dialer.dial().await?);
                    let mux = Arc::new(PicoMux::new(remote_conn.0, remote_conn.1));
                    let mux_dead = Arc::new(AtomicBool::new(false));
                    let mux_dead_evt = Arc::new(async_event::Event::new());
                    let wait_dead = mux_dead_evt.wait_until(|| {
                        if mux_dead.load(Ordering::Relaxed) {
                            Some(anyhow::Ok(()))
                        } else {
                            None
                        }
                    });
                    let accept_loop = async {
                        loop {
                            let client = listener.accept().await?;
                            let mux = mux.clone();
                            let mux_dead = mux_dead.clone();
                            let mux_dead_evt = mux_dead_evt.clone();
                            geph5_rt::spawn(async move {
                                let (mut read_client, mut write_client) = tokio::io::split(client);
                                let _handshake = read_handshake(&mut read_client).await?;
                                write_auth_method(&mut write_client, SocksV5AuthMethod::Noauth)
                                    .await?;
                                let request = read_request(&mut read_client).await?;
                                let port = request.port;
                                let domain: String = match &request.host {
                                    SocksV5Host::Domain(dom) => {
                                        String::from_utf8_lossy(dom).parse()?
                                    }
                                    SocksV5Host::Ipv4(v4) => {
                                        let v4addr = Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3]);
                                        v4addr.to_string()
                                    }
                                    _ => anyhow::bail!("IPv6 not supported"),
                                };
                                let remote_addr = format!("{domain}:{port}");
                                eprintln!("connecting through to {remote_addr}");
                                let stream = mux.open(remote_addr.as_bytes()).await;
                                match stream {
                                    Ok(stream) => {
                                        let (mut read, mut write) = tokio::io::split(stream);
                                        write_request_status(
                                            &mut write_client,
                                            SocksV5RequestStatus::Success,
                                            request.host,
                                            port,
                                        )
                                        .await?;
                                        (
                                            tokio::io::copy(&mut read, &mut write_client),
                                            tokio::io::copy(&mut read_client, &mut write),
                                        )
                                            .race()
                                            .await?;
                                        anyhow::Ok(())
                                    }
                                    Err(err) => {
                                        eprintln!("restarting!!!! {:?}", err);
                                        mux_dead.store(true, Ordering::Relaxed);
                                        mux_dead_evt.notify_one();
                                        anyhow::Ok(())
                                    }
                                }
                            })
                            .detach();
                        }
                    };
                    (wait_dead, accept_loop).race().await?;
                }
            }
        }
    })
}

async fn handle_server(conn: picomux::Stream) -> anyhow::Result<()> {
    // read the destination
    let dest = String::from_utf8_lossy(conn.metadata());
    eprintln!("received conn req for {dest}");
    let remote = tokio::net::TcpStream::connect(&*dest).await?;
    let (mut remote_read, mut remote_write) = remote.into_split();
    let (mut conn_read, mut conn_write) = tokio::io::split(conn);
    (
        tokio::io::copy(&mut remote_read, &mut conn_write),
        tokio::io::copy(&mut conn_read, &mut remote_write),
    )
        .race()
        .await?;
    Ok(())
}
