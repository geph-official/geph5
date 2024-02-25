use std::fmt::Display;

use async_trait::async_trait;
use nanorpc::nanorpc_derive;
mod route;
pub use route::*;
mod exit;
pub use exit::*;
mod signed;
use serde::{Deserialize, Serialize};
pub use signed::*;

mod mac;
pub use mac::*;
mod bridge;
pub use bridge::*;

#[nanorpc_derive]
#[async_trait]
pub trait BrokerProtocol {
    async fn get_exits(&self) -> Result<Signed<ExitList>, GenericError>;
    async fn get_routes(&self, exit: String) -> Result<RouteDescriptor, GenericError>;
    async fn put_exit(&self, descriptor: Mac<Signed<ExitDescriptor>>) -> Result<(), GenericError>;
    async fn put_bridge(&self, descriptor: Mac<BridgeDescriptor>) -> Result<(), GenericError>;
}

pub const DOMAIN_EXIT_DESCRIPTOR: &str = "exit-descriptor";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(transparent)]
pub struct GenericError(pub String);

impl Display for GenericError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Into<anyhow::Error>> From<T> for GenericError {
    fn from(value: T) -> Self {
        Self(value.into().to_string())
    }
}
