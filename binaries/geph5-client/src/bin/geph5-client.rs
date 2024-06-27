use std::path::PathBuf;

use anyhow::bail;
use clap::Parser;
use geph5_client::{Client, Config};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

const CONFIG_FOLDER_NAME: &str = "geph5-client";

/// Run the Geph5 client.
#[derive(Parser)]
struct CliArgs {
    /// path to a YAML-based config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// whether to save the given config file and use it as default
    #[arg(short, long)]
    save_config: bool,

    #[arg(short, long)]
    /// don't start the client, but instead dump authentication info
    dry_run: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_writer(std::io::stderr),
        )
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_client=debug".parse()?)
                .from_env_lossy(),
        )
        .init();

    let args = CliArgs::parse();

    if args.save_config && args.config.is_none() {
        bail!(
            "The save config option can only be used along with the config \
            option"
        );
    }

    let mut config: Config;
    let default_config_path = dirs::config_dir()
        .unwrap()
        .join("geph5-client")
        .join("config.yaml");
    ensure_config_path_exists()?;
    config = match args.config.is_some() {
        true => {
            let config_path = args.config.unwrap();
            let config = parse_config_from_path(config_path.clone())?;
            if args.save_config {
                std::fs::copy(config_path, default_config_path)?;
                tracing::info!("Saved given config as default");
            }
            config
        }
        _ => {
            if default_config_path.exists() {
                tracing::info!("Using default config");
                parse_config_from_path(default_config_path)?
            } else {
                bail!(
                    "Config file wasn't given, and the default config doesn't\
                      exist. Please provide a config for the client to use."
                );
            }
        }
    };
    // Dry run option shouldn't be saved to the config file and
    // should be manually specified everytime the client is run.
    config.dry_run = args.dry_run;
    let client = Client::start(config);
    smolscale::block_on(client.wait_until_dead())?;
    Ok(())
}

fn parse_config_from_path(path: PathBuf) -> anyhow::Result<Config> {
    let parsed_file: serde_json::Value = serde_yaml::from_slice(&std::fs::read(path)?)?;
    let config = serde_json::from_value(parsed_file)?;
    Ok(config)
}

fn ensure_config_path_exists() -> anyhow::Result<()> {
    std::fs::create_dir_all(dirs::config_dir().unwrap().join(CONFIG_FOLDER_NAME))?;
    Ok(())
}
