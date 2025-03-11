use std::path::PathBuf;

use clap::Parser;
use geph5_client::{logging, Client, Config};

/// Run the Geph5 client.
#[derive(Parser)]
struct CliArgs {
    /// path to a YAML-based config file
    #[arg(short, long)]
    config: PathBuf,

    #[arg(short, long)]
    /// don't start the client, but instead dump authentication info
    dry_run: bool,
}

fn main() -> anyhow::Result<()> {
    // Initialize logging with JSON support
    logging::init_logging()?;

    let args = CliArgs::parse();
    let config: serde_json::Value = serde_yaml::from_slice(&std::fs::read(args.config)?)?;
    let mut config: Config = serde_json::from_value(config)?;
    config.dry_run = args.dry_run;
    let client = Client::start(config);
    smolscale::block_on(client.wait_until_dead())?;
    Ok(())
}