use std::{net::SocketAddr, time::SystemTime};

use async_trait::async_trait;
use nanorpc::nanorpc_derive;
use serde::{Deserialize, Serialize};

/// The metadata object passed to the exit on every b2e link.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub struct B2eMetadata {
    pub protocol: ObfsProtocol,
    pub expiry: SystemTime,
}

/// Initialization information for an obfuscation session.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub enum ObfsProtocol {
    Sosistab3(String),
    None,
    ConnTest(Box<Self>),
    PlainTls(Box<Self>),
    Sosistab3New(String, Box<Self>),
}

/// The RPC protocol that bridges expose, called by the broker.
#[nanorpc_derive]
#[async_trait]
pub trait BridgeControlProtocol {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr;
}
