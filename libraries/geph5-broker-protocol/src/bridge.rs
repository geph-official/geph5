use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BridgeDescriptor {
    pub control_listen: SocketAddr,
    pub control_cookie: String,
    pub expiry: u64,
}
