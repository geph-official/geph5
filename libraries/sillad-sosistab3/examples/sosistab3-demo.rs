use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures_concurrency::future::TryJoin;
use sillad::{
    dialer::Dialer,
    listener::Listener,
    tcp::{TcpDialer, TcpListener, TcpPipe},
};
use sillad_sosistab3::{Cookie, SosistabPipe, dialer::SosistabDialer, listener::SosistabListener};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// A simple CLI demo that tunnels TCP traffic through sosistab3.
#[derive(Parser, Debug)]
#[command(name = "sosistab3-demo")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run on the server that is directly connected to the protected service.
    Server(ServerArgs),
    /// Run on the client that exposes a local listening port.
    Client(ClientArgs),
}

#[derive(Args, Debug)]
struct ServerArgs {
    /// Address/port to listen for sosistab3 connections on.
    #[arg(long)]
    listen: SocketAddr,
    /// Address/port of the local TCP service to expose.
    #[arg(long)]
    connect: SocketAddr,
    /// Shared cookie string that must match the client.
    #[arg(long)]
    cookie: String,
}

#[derive(Args, Debug)]
struct ClientArgs {
    /// Address/port of the remote sosistab3 server.
    #[arg(long)]
    connect: SocketAddr,
    /// Local listening address for plain TCP clients.
    #[arg(long)]
    listen: SocketAddr,
    /// Shared cookie string that must match the server.
    #[arg(long)]
    cookie: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    geph5_rt::block_on(async move {
        match cli.command {
            Commands::Server(args) => server_main(args).await,
            Commands::Client(args) => client_main(args).await,
        }
    })
}

async fn server_main(args: ServerArgs) -> Result<()> {
    let ServerArgs {
        listen,
        connect,
        cookie,
    } = args;
    let cookie = Cookie::new(&cookie);
    let tcp_listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind {}", listen))?;
    let bound_addr = tcp_listener.local_addr().await;
    println!(
        "sosistab3 server listening on {}, forwarding to {}",
        bound_addr, connect
    );
    let mut listener = SosistabListener::new(tcp_listener, cookie);
    loop {
        let pipe = listener.accept().await?;
        geph5_rt::spawn(handle_server_pipe(pipe, connect)).detach();
    }
}

async fn handle_server_pipe(pipe: SosistabPipe<TcpPipe>, connect: SocketAddr) -> Result<()> {
    let destination = TcpDialer { dest_addr: connect }
        .dial()
        .await
        .with_context(|| format!("failed to connect to {}", connect))?;
    relay(pipe, destination).await
}

async fn client_main(args: ClientArgs) -> Result<()> {
    let ClientArgs {
        connect,
        listen,
        cookie,
    } = args;
    let cookie = Cookie::new(&cookie);
    let mut local_listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind {}", listen))?;
    let bound_addr = local_listener.local_addr().await;
    println!(
        "client listening on {}, tunneling to {} via sosistab3",
        bound_addr, connect
    );
    loop {
        let conn = local_listener.accept().await?;
        geph5_rt::spawn(handle_client_pipe(conn, connect, cookie)).detach();
    }
}

async fn handle_client_pipe(conn: TcpPipe, connect: SocketAddr, cookie: Cookie) -> Result<()> {
    let dialer = SosistabDialer {
        inner: TcpDialer { dest_addr: connect },
        cookie,
    };
    let sosistab = dialer
        .dial()
        .await
        .with_context(|| format!("failed to connect to {}", connect))?;
    relay(conn, sosistab).await
}

async fn relay<L, R>(left: L, right: R) -> Result<()>
where
    L: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    R: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut left_read, mut left_write) = tokio::io::split(left);
    let (mut right_read, mut right_write) = tokio::io::split(right);

    let left_to_right = async {
        tokio::io::copy(&mut left_read, &mut right_write).await?;
        right_write.shutdown().await?;
        Ok::<_, std::io::Error>(())
    };
    let right_to_left = async {
        tokio::io::copy(&mut right_read, &mut left_write).await?;
        left_write.shutdown().await?;
        Ok::<_, std::io::Error>(())
    };

    (left_to_right, right_to_left).try_join().await?;
    Ok(())
}
