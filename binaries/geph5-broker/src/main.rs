use anyhow::Context;
use axum::{routing::post, Json, Router};
use clap::Parser;
use database::database_gc_loop;
use ed25519_dalek::SigningKey;

use nano_influxdb::InfluxDbEndpoint;
use nanorpc::{JrpcRequest, JrpcResponse, RpcService};
use once_cell::sync::{Lazy, OnceCell};

use rpc_impl::WrappedBrokerService;
use self_stat::self_stat_loop;
use serde::Deserialize;
use smolscale::immortal::{Immortal, RespawnStrategy};
use std::{fmt::Debug, fs, net::SocketAddr, path::PathBuf, sync::LazyLock};
use tikv_jemallocator::Jemalloc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod auth;
mod database;

mod news;
mod payments;
mod puzzle;
mod routes;
mod rpc_impl;
mod self_stat;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// The master secret.
static MASTER_SECRET: Lazy<SigningKey> = Lazy::new(|| {
    let sk = SigningKey::from_bytes(
        std::fs::read(&CONFIG_FILE.wait().master_secret)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap(),
    );
    let pk = sk.verifying_key();
    tracing::info!("*** master PK = {} ***", hex::encode(pk.as_bytes()));
    sk
});

/// The Plus mizaru SK.
static PLUS_MIZARU_SK: Lazy<mizaru2::SecretKey> = Lazy::new(|| {
    let mizaru = load_mizaru_sk("plus.bin");
    let pk = mizaru.to_public_key().to_bytes();
    tracing::info!("*** Plus Mizaru PK = {} ***", hex::encode(pk));
    mizaru
});

/// The Free mizaru SK.
static FREE_MIZARU_SK: Lazy<mizaru2::SecretKey> = Lazy::new(|| {
    let mizaru = load_mizaru_sk("free.bin");
    let pk = mizaru.to_public_key().to_bytes();
    tracing::info!("*** Free Mizaru PK = {} ***", hex::encode(pk));
    mizaru
});

fn load_mizaru_sk(name: &str) -> mizaru2::SecretKey {
    let mizaru_keys_dir = &CONFIG_FILE.wait().mizaru_keys;
    let plus_file_path = mizaru_keys_dir.join(name);

    if plus_file_path.exists() {
        // If the file exists, read it
        let file_content = fs::read(&plus_file_path).unwrap();
        stdcode::deserialize(&file_content).unwrap()
    } else {
        // If the file doesn't exist, generate a new secret key and write it to the file
        let new_key = mizaru2::SecretKey::generate(name);
        if let Some(parent) = plus_file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&plus_file_path, stdcode::serialize(&new_key).unwrap()).unwrap();
        new_key
    }
}

/// This struct defines the structure of our configuration file
#[derive(Deserialize)]
struct ConfigFile {
    listen: SocketAddr,
    tcp_listen: SocketAddr,
    master_secret: PathBuf,
    mizaru_keys: PathBuf,
    postgres_url: String,
    #[serde(default)]
    postgres_root_cert: Option<PathBuf>,

    bridge_token: String,
    exit_token: String,

    #[serde(default = "default_puzzle_difficulty")]
    puzzle_difficulty: u16,

    #[serde(default)]
    statsd_addr: Option<SocketAddr>,

    openai_key: String,

    #[serde(default = "default_payment_service")]
    payment_url: String,

    #[serde(default)]
    payment_support_secret: String,

    /// Optional InfluxDB configuration for metrics
    #[serde(default)]
    influxdb: Option<InfluxDbEndpoint>,
}

fn default_puzzle_difficulty() -> u16 {
    24
}

fn default_payment_service() -> String {
    "https://web-backend.geph.io/rpc".to_string()
}

/// Run the Geph5 broker.
#[derive(Parser)]
struct CliArgs {
    /// path to a YAML-based config file
    #[arg(short, long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_broker=debug".parse()?)
                .from_env_lossy(),
        )
        .init();
    // Parse the command-line arguments
    let args = CliArgs::parse();

    // Read the content of the YAML file
    let config_contents =
        fs::read_to_string(args.config).context("Failed to read the config file")?;

    // Parse the YAML file into our AppConfig struct
    let config: ConfigFile =
        serde_yaml::from_str(&config_contents).context("Failed to parse the config file")?;

    // Log if InfluxDB is configured
    if let Some(influxdb) = &config.influxdb {
        tracing::info!("InfluxDB endpoint configured at {}", influxdb.url);
    } else {
        tracing::info!("No InfluxDB endpoint configured");
    }

    let _ = CONFIG_FILE.set(config);

    Lazy::force(&PLUS_MIZARU_SK);
    Lazy::force(&FREE_MIZARU_SK);
    LazyLock::force(&database::POSTGRES);

    let _gc_loop = Immortal::respawn(RespawnStrategy::Immediate, database_gc_loop);
    let _self_stat_loop = Immortal::respawn(RespawnStrategy::Immediate, self_stat_loop);
    let _tcp_loop = Immortal::respawn(RespawnStrategy::Immediate, || async {
        nanorpc_sillad::rpc_serve(
            sillad::tcp::TcpListener::bind(CONFIG_FILE.wait().tcp_listen).await?,
            WrappedBrokerService::new(),
        )
        .await?;
        anyhow::Ok(())
    });

    let listener = tokio::net::TcpListener::bind(CONFIG_FILE.wait().listen).await?;
    let app = Router::new().route("/", post(rpc));
    axum::serve(listener, app).await?;
    Ok(())
}

async fn rpc(Json(payload): Json<JrpcRequest>) -> Json<JrpcResponse> {
    Json(WrappedBrokerService::new().respond_raw(payload).await)
}

fn log_error(e: &impl Debug) {
    tracing::warn!(err = debug(e), "transient error")
}
