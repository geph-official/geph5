use argh::FromArgs;
use futures_lite::FutureExt;
use futures_util::{AsyncReadExt, TryFutureExt};
use picomux::PicoMux;

use sillad::{
    dialer::Dialer,
    listener::Listener,
    tcp::{TcpDialer, TcpListener},
};
use sillad_sosistab3::{dialer::SosistabDialer, listener::SosistabListener, Cookie};
use socksv5::v5::{
    read_handshake, read_request, write_auth_method, write_request_status, SocksV5AuthMethod,
    SocksV5Host, SocksV5RequestStatus,
};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// SOCKS5 Program
#[derive(FromArgs)]
struct Socks5 {
    #[argh(subcommand)]
    nested: SubCommands,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum SubCommands {
    Server(ServerCommand),
    Client(ClientCommand),
}

/// Server subcommand
/// Listens on the specified SocketAddr.
#[derive(FromArgs)]
#[argh(subcommand, name = "server")]
struct ServerCommand {
    /// socket address to bind the server (e.g., "127.0.0.1:1080")
    #[argh(option)]
    listen: SocketAddr,
}

/// Client subcommand
/// Connects to a specified SOCKS5 server.
#[derive(FromArgs)]
#[argh(subcommand, name = "client")]
struct ClientCommand {
    /// socket address to bind the socks5 server (e.g., "127.0.0.1:1080")
    #[argh(option)]
    listen: SocketAddr,
    /// server address and port to connect to (e.g., "127.0.0.1:1080")
    #[argh(option)]
    connect: SocketAddr,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    smolscale::block_on(async {
        let socks5: Socks5 = argh::from_env();
        match socks5.nested {
            SubCommands::Server(server) => {
                eprintln!("Starting server on {}", server.listen);
                let listener = TcpListener::bind(server.listen).await?;
                let mut listener = SosistabListener::new(listener, Some(Cookie::new("hello")));
                loop {
                    let tcp_stream = listener.accept().await?;
                    let (read, write) = tcp_stream.split();
                    let mut mux = PicoMux::new(read, write);
                    smolscale::spawn::<anyhow::Result<()>>(async move {
                        loop {
                            let client = mux.accept().await?;
                            smolscale::spawn(
                                handle_server(client)
                                    .map_err(|e| eprintln!("server handling dying: {:?}", e)),
                            )
                            .detach();
                        }
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
                    let remote_conn = dialer.dial().await?.split();
                    let mux = Arc::new(PicoMux::new(remote_conn.0, remote_conn.1));
                    let mux_dead = Arc::new(AtomicBool::new(false));
                    let mux_dead_evt = Arc::new(async_event::Event::new());
                    mux_dead_evt
                        .wait_until(|| {
                            if mux_dead.load(Ordering::Relaxed) {
                                Some(anyhow::Ok(()))
                            } else {
                                None
                            }
                        })
                        .race(async {
                            loop {
                                let client = listener.accept().await?;
                                let mux = mux.clone();
                                let mux_dead = mux_dead.clone();
                                let mux_dead_evt = mux_dead_evt.clone();
                                smolscale::spawn(async move {
                                    let (mut read_client, mut write_client) = client.split();
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
                                            let (read, write) = stream.split();
                                            write_request_status(
                                                &mut write_client,
                                                SocksV5RequestStatus::Success,
                                                request.host,
                                                port,
                                            )
                                            .await?;
                                            smol::io::copy(read, write_client)
                                                .race(smol::io::copy(read_client, write))
                                                .await?;
                                            Ok(())
                                        }
                                        Err(err) => {
                                            eprintln!("restarting!!!! {:?}", err);
                                            mux_dead.store(true, Ordering::Relaxed);
                                            mux_dead_evt.notify_one();
                                            Ok(())
                                        }
                                    }
                                })
                                .detach();
                            }
                        })
                        .await?;
                }
            }
        }
    })
}

async fn handle_server(conn: picomux::Stream) -> anyhow::Result<()> {
    // read the destination
    let dest = String::from_utf8_lossy(conn.metadata());
    eprintln!("received conn req for {dest}");
    let remote = smol::net::TcpStream::connect(&*dest).await?;
    let (conn_read, conn_write) = conn.split();
    smol::io::copy(remote.clone(), conn_write)
        .race(smol::io::copy(conn_read, remote))
        .await?;
    Ok(())
}
