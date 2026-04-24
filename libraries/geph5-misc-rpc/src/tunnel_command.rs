use std::{fmt, net::SocketAddr, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub enum TunnelCommand {
    Legacy { protocol: String, host: String },
    Rich(RichTunnelCommand),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RichTunnelCommand {
    pub protocol: String,
    pub host: String,
}

impl TunnelCommand {
    pub fn protocol(&self) -> &str {
        match self {
            Self::Legacy { protocol, .. } => protocol,
            Self::Rich(r) => &r.protocol,
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Self::Legacy { host, .. } => host,
            Self::Rich(r) => &r.host,
        }
    }
}

impl fmt::Display for TunnelCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy { protocol, host } => write!(f, "{protocol}${host}"),
            Self::Rich(r) => write!(f, "{}", serde_json::to_string(r).unwrap()),
        }
    }
}

impl FromStr for TunnelCommand {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with('{') {
            let rich: RichTunnelCommand = serde_json::from_str(s)
                .map_err(|e| anyhow::anyhow!("failed to parse rich tunnel command: {e}"))?;
            Ok(Self::Rich(rich))
        } else if let Some((protocol, host)) = s.split_once('$') {
            Ok(Self::Legacy {
                protocol: protocol.to_string(),
                host: host.to_string(),
            })
        } else {
            Ok(Self::Legacy {
                protocol: "tcp".to_string(),
                host: s.to_string(),
            })
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RichTunnelResponse {
    pub resolved_addr: SocketAddr,
    pub open_ms: Option<u32>,
}
