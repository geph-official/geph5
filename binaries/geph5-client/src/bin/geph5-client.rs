use std::{
    io::{self, stdin, stdout, Read, Write},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use geph5_client::{Client, Config};
use nanorpc::{JrpcRequest, RpcTransport};

/// Run the Geph5 client.
#[derive(Parser)]
struct CliArgs {
    /// path to a YAML-based config file
    #[arg(short, long)]
    config: PathBuf,

    #[arg(short, long)]
    /// do RPC on the stdio
    stdio_rpc: bool,

    #[arg(long)]
    /// Use stdin/stdout as a VPN interface with 16-bit big-endian length prefixes
    stdio_vpn: bool,
}

fn main() -> anyhow::Result<()> {
    smolscale::permanently_single_threaded();

    let args = CliArgs::parse();
    let config: serde_json::Value = serde_yaml::from_slice(&std::fs::read(args.config)?)?;
    let config: Config = serde_json::from_value(config)?;
    let client = Client::start(config);

    if args.stdio_rpc {
        let rpc = Arc::new(client.control_client().0);
        std::thread::spawn(move || {
            let stdin = stdin();
            for line in stdin.lines() {
                let line = line.unwrap();
                let line: JrpcRequest = serde_json::from_str(&line).unwrap();
                let rpc = rpc.clone();
                let resp = smolscale::block_on(async move { rpc.call_raw(line).await }).unwrap();
                println!("{}", serde_json::to_string(&resp).unwrap());
            }
        });
    } else if args.stdio_vpn {
        // Run the stdio VPN interface
        run_stdio_vpn(client.clone())?;
    }

    smolscale::block_on(client.wait_until_dead())?;
    Ok(())
}

/// Run the stdio VPN interface where packets are read from stdin and written to stdout,
/// each prefixed with a 16-bit big-endian length.
fn run_stdio_vpn(client: Client) -> anyhow::Result<()> {
    // Spawn a thread for reading from stdin and sending to VPN
    let client_clone = client.clone();
    std::thread::spawn(move || -> anyhow::Result<()> {
        let mut stdin = stdin().lock();
        let mut length_buf = [0u8; 2];
        loop {
            // Read the 16-bit big-endian length prefix
            match stdin.read_exact(&mut length_buf) {
                Ok(_) => {
                    // Convert big-endian length to usize
                    let length = u16::from_be_bytes(length_buf) as usize;

                    // Read the packet data
                    let mut packet_buf = vec![0u8; length];
                    stdin.read_exact(&mut packet_buf)?;

                    // Send the packet to the VPN
                    smol::future::block_on(client_clone.send_vpn_packet(Bytes::from(packet_buf)))?;
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    // End of input stream
                    break;
                }
                Err(e) => {
                    return Err(e).context("Error reading from stdin");
                }
            }
        }
        Ok(())
    });

    // Spawn a thread for receiving from VPN and writing to stdout
    std::thread::spawn(move || -> anyhow::Result<()> {
        let mut stdout = stdout().lock();
        loop {
            // Receive a packet from the VPN
            let packet = smol::future::block_on(client.recv_vpn_packet())?;

            // Get the length as a 16-bit value, capping at u16::MAX if larger
            let length = std::cmp::min(packet.len(), u16::MAX as usize) as u16;

            // Write the 16-bit big-endian length prefix
            stdout.write_all(&length.to_be_bytes())?;

            // Write the packet data
            stdout.write_all(&packet)?;
            stdout.flush()?;
        }
    });

    Ok(())
}
