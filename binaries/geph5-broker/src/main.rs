use anyhow::Context;
use argh::FromArgs;
use axum::{routing::post, Json, Router};
use database::database_gc_loop;
use ed25519_dalek::SigningKey;
use geph5_broker_protocol::BrokerService;
use nanorpc::{JrpcRequest, JrpcResponse, RpcService};
use once_cell::sync::{Lazy, OnceCell};
use rpc_impl::BrokerImpl;
use serde::Deserialize;
use smolscale::immortal::{Immortal, RespawnStrategy};
use std::{fmt::Debug, fs, net::SocketAddr, path::PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod auth;
mod database;
mod routes;
mod rpc_impl;

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// The master secret.
static MASTER_SECRET: Lazy<SigningKey> = Lazy::new(|| {
    SigningKey::from_bytes(
        std::fs::read(&CONFIG_FILE.wait().master_secret)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap(),
    )
});

/// The Plus mizaru SK.
static PLUS_MIZARU_SK: Lazy<mizaru2::SecretKey> = Lazy::new(|| load_mizaru_sk("plus.bin"));

/// The Free mizaru SK.
static FREE_MIZARU_SK: Lazy<mizaru2::SecretKey> = Lazy::new(|| load_mizaru_sk("free.bin"));

fn load_mizaru_sk(name: &str) -> mizaru2::SecretKey {
    let mizaru_keys_dir = &CONFIG_FILE.wait().mizaru_keys;
    let plus_file_path = mizaru_keys_dir.join(name);

    if plus_file_path.exists() {
        // If the file exists, read it
        let file_content = fs::read(&plus_file_path).unwrap();
        stdcode::deserialize(&file_content).unwrap()
    } else {
        // If the file doesn't exist, generate a new secret key and write it to the file
        let (key_count, key_bits) = {
            let cfg = &CONFIG_FILE.wait();
            (cfg.mizaru_key_count, cfg.mizaru_key_bits)
        };
        let new_key = mizaru2::SecretKey::generate(name, key_count, key_bits);
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
    #[serde(default = "default_mizaru_key_count")]
    mizaru_key_count: usize,
    #[serde(default = "default_mizaru_key_bits")]
    mizaru_key_bits: usize,
    postgres_url: String,
    #[serde(default)]
    postgres_root_cert: Option<PathBuf>,

    bridge_token: String,
    exit_token: String,

    #[serde(default)]
    statsd_addr: Option<SocketAddr>,
}

fn default_mizaru_key_count() -> usize {
    mizaru2::KEY_COUNT
}

fn default_mizaru_key_bits() -> usize {
    mizaru2::KEY_BITS
}

/// Run the Geph5 broker.
#[derive(FromArgs)]
struct CliArgs {
    /// path to a YAML-based config file
    #[argh(option, short = 'c')]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            EnvFilter::builder()
                .with_default_directive("geph5_broker".parse()?)
                .from_env_lossy(),
        )
        .init();
    // Parse the command-line arguments
    let args: CliArgs = argh::from_env();

    // Read the content of the YAML file
    let config_contents =
        fs::read_to_string(args.config).context("Failed to read the config file")?;

    // Parse the YAML file into our AppConfig struct
    let config: ConfigFile =
        serde_yaml::from_str(&config_contents).context("Failed to parse the config file")?;

    let _ = CONFIG_FILE.set(config);

    Lazy::force(&PLUS_MIZARU_SK);
    Lazy::force(&FREE_MIZARU_SK);
    Lazy::force(&database::POSTGRES);
    database::init_schema().await?;

    let _gc_loop = Immortal::respawn(RespawnStrategy::Immediate, database_gc_loop);

    let _tcp_loop = Immortal::respawn(RespawnStrategy::Immediate, || async {
        nanorpc_sillad::rpc_serve(
            sillad::tcp::TcpListener::bind(CONFIG_FILE.wait().tcp_listen).await?,
            BrokerService(BrokerImpl {}),
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
    Json(BrokerService(BrokerImpl {}).respond_raw(payload).await)
}

fn log_error(e: &impl Debug) {
    tracing::warn!(err = debug(e), "transient error")
}
