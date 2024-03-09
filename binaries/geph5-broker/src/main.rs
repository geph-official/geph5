use anyhow::Context;
use argh::FromArgs;
use axum::{routing::post, Json, Router};
use database::database_gc_loop;
use geph5_broker_protocol::BrokerService;
use nanorpc::{JrpcRequest, JrpcResponse, RpcService};
use once_cell::sync::OnceCell;
use rpc_impl::BrokerImpl;
use serde::Deserialize;
use smolscale::immortal::{Immortal, RespawnStrategy};
use std::{fs, net::SocketAddr, path::PathBuf};

mod database;
mod routes;
mod rpc_impl;

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// This struct defines the structure of our configuration file
#[derive(Deserialize)]
struct ConfigFile {
    listen: SocketAddr,
    tcp_listen: SocketAddr,
    master_secret: PathBuf,
    postgres_url: String,
    postgres_root_cert: PathBuf,

    bridge_token: String,
    exit_token: String,
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
    tracing_subscriber::fmt::init();
    // Parse the command-line arguments
    let args: CliArgs = argh::from_env();

    // Read the content of the YAML file
    let config_contents =
        fs::read_to_string(args.config).context("Failed to read the config file")?;

    // Parse the YAML file into our AppConfig struct
    let config: ConfigFile =
        serde_yaml::from_str(&config_contents).context("Failed to parse the config file")?;

    let _ = CONFIG_FILE.set(config);

    let _gc_loop = Immortal::respawn(RespawnStrategy::Immediate, database_gc_loop);

    let _tcp_loop = smolscale::spawn(nanorpc_sillad::rpc_serve(
        sillad::tcp::TcpListener::bind(CONFIG_FILE.wait().tcp_listen).await?,
        BrokerService(BrokerImpl {}),
    ));

    let listener = tokio::net::TcpListener::bind(CONFIG_FILE.wait().listen).await?;
    let app = Router::new().route("/", post(rpc));
    axum::serve(listener, app).await?;
    Ok(())
}

async fn rpc(Json(payload): Json<JrpcRequest>) -> Json<JrpcResponse> {
    Json(BrokerService(BrokerImpl {}).respond_raw(payload).await)
}
