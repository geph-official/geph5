use anyhow::Context;
use argh::FromArgs;
use once_cell::sync::OnceCell;
use serde::Deserialize;
use std::fs;

mod database;
mod rpc_impl;

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// This struct defines the structure of our configuration file
#[derive(Deserialize)]
struct ConfigFile {
    postgres_url: String,
    postgres_root_cert: String,

    bridge_token: String,
}

/// Run the Geph5 broker.
#[derive(FromArgs)]
struct CliArgs {
    /// path to a YAML-based config file
    #[argh(option, short = 'c')]
    config: String,
}

fn main() -> anyhow::Result<()> {
    // Parse the command-line arguments
    let args: CliArgs = argh::from_env();

    // Read the content of the YAML file
    let config_contents =
        fs::read_to_string(args.config).context("Failed to read the config file")?;

    // Parse the YAML file into our AppConfig struct
    let config: ConfigFile =
        serde_yaml::from_str(&config_contents).context("Failed to parse the config file")?;

    let _ = CONFIG_FILE.set(config);

    Ok(())
}
