use std::{io::stdin, path::PathBuf, sync::Arc};

use clap::Parser;
use geph5_client::{logging, Client, Config};
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
}

fn main() -> anyhow::Result<()> {
    smolscale::permanently_single_threaded();
    // Initialize logging with JSON support
    logging::init_logging()?;

    let args = CliArgs::parse();
    let config: serde_json::Value = serde_yaml::from_slice(&std::fs::read(args.config)?)?;
    let config: Config = serde_json::from_value(config)?;
    let client = Client::start(config);
    let rpc = Arc::new(client.control_client().0);
    if args.stdio_rpc {
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
    }
    smolscale::block_on(client.wait_until_dead())?;
    Ok(())
}
