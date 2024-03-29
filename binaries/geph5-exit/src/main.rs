use argh::FromArgs;
use ed25519_dalek::SigningKey;
use isocountry::CountryCode;
use listen::listen_main;
use once_cell::sync::{Lazy, OnceCell};
use rand::Rng;
use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

mod broker;
mod listen;
mod proxy;

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// This struct defines the structure of our configuration file
#[derive(Deserialize)]
struct ConfigFile {
    signing_secret: PathBuf,
    broker: Option<BrokerConfig>,

    c2e_listen: SocketAddr,
    b2e_listen: SocketAddr,

    country: CountryCode,
    city: String,
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
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive("geph5_exit=debug".parse()?)
                .from_env_lossy(),
        )
        .init();
    let args: CliArgs = argh::from_env();
    CONFIG_FILE
        .set(serde_yaml::from_slice(&std::fs::read(args.config)?)?)
        .ok()
        .unwrap();
    smolscale::block_on(listen_main())
}
