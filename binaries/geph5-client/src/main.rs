use std::path::PathBuf;

use argh::FromArgs;
use client::{Client, Config};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod broker;
mod client;
mod exit;

/// Run the Geph5 broker.
#[derive(FromArgs)]
struct CliArgs {
    /// path to a YAML-based config file
    #[argh(option, short = 'c')]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_client=debug".parse()?)
                .from_env_lossy(),
        )
        .init();
    smolscale::permanently_single_threaded();
    let args: CliArgs = argh::from_env();
    let config: serde_json::Value = serde_yaml::from_slice(&std::fs::read(args.config)?)?;
    let config: Config = serde_json::from_value(config)?;
    let client = Client::start(config);
    smolscale::block_on(client.wait_until_dead())?;
    Ok(())
}
