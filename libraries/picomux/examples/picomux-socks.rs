use argh::FromArgs;
use futures_lite::{AsyncWriteExt, FutureExt};
use futures_util::AsyncReadExt;
use picomux::PicoMux;
use smol::net::{TcpListener, TcpStream};
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
    connect: String,
}

fn main() -> anyhow::Result<()> {
    smolscale::block_on(async {
        let socks5: Socks5 = argh::from_env();
        match socks5.nested {
            SubCommands::Server(server) => {
                eprintln!("Starting server on {}", server.listen);
                let listener = TcpListener::bind(server.listen).await?;
                loop {
                    let (tcp_stream, _) = listener.accept().await?;
                    let mut mux = PicoMux::new(tcp_stream.clone(), tcp_stream);
                    smolscale::spawn::<anyhow::Result<()>>(async move {
                        loop {
                            let client = mux.accept().await?;
                            smolscale::spawn(handle_server(client)).detach();
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
                let listener = TcpListener::bind(client.listen).await?;
                loop {
                    let remote_conn = TcpStream::connect(&client.connect).await?;
                    let mux = Arc::new(PicoMux::new(remote_conn.clone(), remote_conn));
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
                                let (client, _) = listener.accept().await?;
                                let mux = mux.clone();
                                let mux_dead = mux_dead.clone();
                                let mux_dead_evt = mux_dead_evt.clone();
                                smolscale::spawn(async move {
                                    let _handshake = read_handshake(client.clone()).await?;
                                    write_auth_method(client.clone(), SocksV5AuthMethod::Noauth)
                                        .await?;
                                    let request = read_request(client.clone()).await?;
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
                                    let stream = mux.open().await;
                                    if let Ok(stream) = stream {
                                        let (read, mut write) = stream.split();
                                        write
                                            .write_all(
                                                &(remote_addr.as_bytes().len() as u16)
                                                    .to_be_bytes(),
                                            )
                                            .await?;
                                        write.write_all(remote_addr.as_bytes()).await?;
                                        write_request_status(
                                            client.clone(),
                                            SocksV5RequestStatus::Success,
                                            request.host,
                                            port,
                                        )
                                        .await?;
                                        smol::io::copy(read, client.clone())
                                            .race(smol::io::copy(client, write))
                                            .await?;
                                        Ok(())
                                    } else {
                                        eprintln!("restarting!!!!");
                                        mux_dead.store(true, Ordering::Relaxed);
                                        mux_dead_evt.notify_one();
                                        Ok(())
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

async fn handle_server(mut conn: picomux::Stream) -> anyhow::Result<()> {
    // read the destination
    let mut dest_len_buf = [0u8; 2];
    conn.read_exact(&mut dest_len_buf).await?;
    let dest_len = u16::from_be_bytes(dest_len_buf);
    let mut dest = vec![0u8; dest_len as usize];
    conn.read_exact(&mut dest).await?;
    let dest = String::from_utf8_lossy(&dest);
    eprintln!("received conn req for {dest}");
    let remote = TcpStream::connect(&*dest).await?;
    let (conn_read, conn_write) = conn.split();
    smol::io::copy(remote.clone(), conn_write)
        .race(smol::io::copy(conn_read, remote))
        .await?;
    Ok(())
}
