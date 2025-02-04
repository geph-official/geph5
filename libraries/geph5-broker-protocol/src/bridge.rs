use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Hash, PartialEq, Eq)]
pub struct BridgeDescriptor {
    pub control_listen: SocketAddr,
    pub control_cookie: String,
    pub pool: String,
    pub expiry: u64,
}
