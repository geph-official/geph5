use clap::Parser;
use ed25519_dalek::SigningKey;
use isocountry::CountryCode;
use listen::listen_main;
use once_cell::sync::{Lazy, OnceCell};
use rand::Rng;
use serde::Deserialize;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

mod broker;
mod listen;
mod proxy;
mod ratelimit;

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

use crate::ratelimit::update_load_loop;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// This struct defines the structure of our configuration file
#[derive(Deserialize)]
struct ConfigFile {
    signing_secret: PathBuf,
    broker: Option<BrokerConfig>,

    c2e_listen: SocketAddr,
    b2e_listen: SocketAddr,
    ip_addr: Option<IpAddr>,

    country: CountryCode,
    city: String,

    #[serde(default = "default_free_ratelimit")]
    free_ratelimit: u32,

    #[serde(default = "default_plus_ratelimit")]
    plus_ratelimit: u32,

    #[serde(default = "default_total_ratelimit")]
    total_ratelimit: u32,
}

fn default_free_ratelimit() -> u32 {
    500
}

fn default_plus_ratelimit() -> u32 {
    100000
}

fn default_total_ratelimit() -> u32 {
    100000
}

#[derive(Deserialize)]
struct BrokerConfig {
    url: String,
    auth_token: String,
}

static SIGNING_SECRET: Lazy<SigningKey> = Lazy::new(|| {
    let config_file = CONFIG_FILE.get().expect("Config file must be initialized.");
    let path = &config_file.signing_secret;
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => {
            let bytes: [u8; 32] = bytes.as_slice().try_into().unwrap();
            SigningKey::from_bytes(&bytes)
        }
        _ => {
            // Generate a new SigningKey if there's an error or the length is not 32 bytes.
            let new_key = SigningKey::from_bytes(&rand::thread_rng().gen());
            let key_bytes = new_key.to_bytes();
            std::fs::write(&config_file.signing_secret, key_bytes).unwrap();
            new_key
        }
    }
});

/// Run the Geph5 broker.
#[derive(Parser)]
struct CliArgs {
    /// path to a YAML-based config file
    #[arg(short, long)]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    std::thread::spawn(update_load_loop);
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive("geph5_exit=debug".parse()?)
                .from_env_lossy(),
        )
        .init();
    tracing::info!("**** START GEPH EXIT ****");
    let args = CliArgs::parse();
    let config: ConfigFile = serde_yaml::from_slice(&std::fs::read(args.config)?)?;

    CONFIG_FILE.set(config).ok().unwrap();

    smolscale::block_on(listen_main())
}
