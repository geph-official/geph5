mod bw_accounting;
mod dns;
mod ipv6;
mod session;
mod tasklimit;

use clap::Parser;
use ed25519_dalek::SigningKey;
use ipnet::Ipv6Net;

use isocountry::CountryCode;
use listen::listen_main;
use once_cell::sync::{Lazy, OnceCell};
use rand::Rng;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

mod allow;
mod auth;
mod broker;
mod listen;
mod proxy;
mod ratelimit;
mod schedlag;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use crate::ratelimit::update_load_loop;
use geph5_broker_protocol::{AccountLevel, ExitCategory, ExitMetadata};

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// This struct defines the structure of our configuration file
#[serde_as]
#[derive(Deserialize)]
struct ConfigFile {
    signing_secret: PathBuf,
    broker: Option<BrokerConfig>,

    c2e_listen: SocketAddr,
    b2e_listen: SocketAddr,
    ip_addr: Option<IpAddr>,

    country: CountryCode,
    city: String,

    #[serde(default)]
    metadata: Option<ExitMetadata>,

    #[serde(default = "default_country_blacklist")]
    country_blacklist: Vec<String>,

    #[serde(default = "default_free_ratelimit")]
    free_ratelimit: u32,

    #[serde(default = "default_plus_ratelimit")]
    plus_ratelimit: u32,

    #[serde(default = "default_total_ratelimit")]
    total_ratelimit: u32,

    #[serde(default = "default_free_port_whitelist")]
    free_port_whitelist: Vec<u16>,

    #[serde(default = "default_task_limit")]
    task_limit: usize,

    #[serde_as(as = "DisplayFromStr")]
    #[serde(default)]
    ipv6_subnet: Ipv6Net,

    #[serde(default = "default_ipv6_pool_size")]
    ipv6_pool_size: usize,
}

fn default_free_ratelimit() -> u32 {
    150
}

fn default_plus_ratelimit() -> u32 {
    50000
}

fn default_total_ratelimit() -> u32 {
    125000
}

fn default_task_limit() -> usize {
    1_000_000
}

fn default_free_port_whitelist() -> Vec<u16> {
    vec![80, 443, 8080, 8443, 22, 53]
}

fn default_country_blacklist() -> Vec<String> {
    vec![]
}

fn default_ipv6_pool_size() -> usize {
    100
}

fn default_exit_metadata() -> ExitMetadata {
    let cfg = CONFIG_FILE.wait();
    let mut allowed_levels = vec![AccountLevel::Plus];
    if cfg.free_ratelimit > 0
        && matches!(
            cfg.country,
            CountryCode::CAN
                | CountryCode::NLD
                | CountryCode::FRA
                | CountryCode::POL
                | CountryCode::DEU
        )
    {
        allowed_levels.push(AccountLevel::Free);
    }
    ExitMetadata {
        allowed_levels,
        category: ExitCategory::Core,
    }
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

fn exit_metadata() -> ExitMetadata {
    let cfg = CONFIG_FILE.wait();
    let mut metadata = cfg.metadata.clone().unwrap_or_else(default_exit_metadata);
    if cfg.free_ratelimit == 0 {
        metadata
            .allowed_levels
            .retain(|level| *level != AccountLevel::Free);
    }
    metadata
}

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

    smol::future::block_on(smolscale::spawn(listen_main()))
}
