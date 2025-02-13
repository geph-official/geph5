use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
/// This fully describes a route to a particular exit.
pub enum RouteDescriptor {
    Tcp(SocketAddr),
    Sosistab3 {
        cookie: String,
        lower: Box<RouteDescriptor>,
    },
    PlainTls {
        sni_domain: Option<String>,
        lower: Box<RouteDescriptor>,
    },
    Race(Vec<RouteDescriptor>),
    Fallback(Vec<RouteDescriptor>),
    Timeout {
        milliseconds: u32,
        lower: Box<RouteDescriptor>,
    },
    Delay {
        milliseconds: u32,
        lower: Box<RouteDescriptor>,
    },
    ConnTest {
        ping_count: u32,
        lower: Box<RouteDescriptor>,
    },

    #[serde(untagged)]
    Other(serde_json::Value),
}
