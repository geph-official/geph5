//! Plain-data types describing how to **configure** a geph5-client engine: its
//! [`Config`] plus the broker-source descriptors its config embeds.
//!
//! These live here, alongside the engine's control protocol
//! ([`crate::client_control`]), so that tools which merely *drive or configure* an
//! engine (the `geph` daemon/CLI) can depend on this lightweight crate instead of
//! the entire engine. The behavior that turns these descriptors into live
//! transports stays in `geph5-client` (as extension traits).

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use geph5_broker_protocol::{Credential, ExitConstraint};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
// Reject unknown fields so a stale config from before the VPN refactor — which
// carried `vpn`/`vpn_fd` keys that no longer exist — fails loudly instead of
// silently parsing and running proxy-only (the old fields were ignored, so a
// user who expected a system tunnel would have leaked outside it).
#[serde(deny_unknown_fields)]
pub struct Config {
    pub socks5_listen: Option<SocketAddr>,
    pub http_proxy_listen: Option<SocketAddr>,
    pub pac_listen: Option<SocketAddr>,

    pub control_listen: Option<SocketAddr>,
    #[serde(default)]
    pub control_listen_unix: Option<PathBuf>,
    /// Windows named pipe (e.g. `\\.\pipe\geph-engine-control`) to serve the
    /// control protocol on. The Windows analogue of `control_listen_unix`.
    #[serde(default)]
    pub control_listen_pipe: Option<String>,
    pub exit_constraint: ExitConstraint,
    #[serde(default)]
    pub allow_direct: bool,

    pub cache: Option<PathBuf>,

    pub broker: Option<BrokerSource>,
    #[serde(alias = "tunneled_broker_source")]
    pub tunneled_broker: Option<TunneledBrokerSource>,
    pub broker_keys: Option<BrokerKeys>,

    #[serde(default)]
    pub port_forward: Vec<PortForwardCfg>,

    #[serde(default)]
    pub spoof_dns: bool,
    #[serde(default)]
    pub passthrough_china: bool,
    /// Whether to let connections to private/LAN addresses bypass the tunnel
    /// (connect directly). Defaults to true. Set false for a strict full tunnel.
    #[serde(default = "default_allow_lan")]
    pub allow_lan: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub credentials: Credential,

    #[serde(default)]
    pub sess_metadata: serde_json::Value,
    pub task_limit: Option<u32>,
}

fn default_allow_lan() -> bool {
    true
}

impl Config {
    /// Create an "inert" version of this config that does not start any processes.
    pub fn inert(&self) -> Self {
        let mut this = self.clone();
        this.dry_run = true;
        this.socks5_listen = None;
        this.http_proxy_listen = None;
        this.pac_listen = None;
        this.control_listen = None;
        this.control_listen_unix = None;
        this.control_listen_pipe = None;
        this
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PortForwardCfg {
    pub listen: SocketAddr,
    pub connect: String,
}

#[derive(Serialize, Deserialize, Clone)]
/// Broker keys, in hexadecimal format.
pub struct BrokerKeys {
    pub master: String,
    pub mizaru_free: String,
    pub mizaru_plus: String,
    pub mizaru_bw: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSource {
    Direct(String),
    Fronted {
        front: String,
        host: String,
        #[serde(default)]
        override_dns: Option<Vec<SocketAddr>>,
    },
    DirectTcp(SocketAddr),
    AwsLambda {
        function_name: String,
        region: String,
        obfs_key: String,
    },
    Race(Vec<BrokerSource>),
    PriorityRace(BTreeMap<u64, BrokerSource>),
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum TunneledBrokerSource {
    Direct(String),
}
