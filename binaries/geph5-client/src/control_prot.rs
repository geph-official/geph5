use std::time::SystemTime;

use async_trait::async_trait;
use isocountry::CountryCode;
use nanorpc::nanorpc_derive;
use serde::{Deserialize, Serialize};

#[nanorpc_derive]
#[async_trait]
pub trait ControlProtocol {
    async fn conn_info(&self) -> ConnInfo;
    async fn stat_num(&self, stat: String) -> f64;
    async fn start_time(&self) -> SystemTime;
}

#[derive(Serialize, Deserialize)]
pub enum ConnInfo {
    Connecting,
    Connected(ConnectedInfo),
}

#[derive(Serialize, Deserialize)]
pub struct ConnectedInfo {
    pub protocol: String,
    pub bridge: String,
    pub exit: String,
    pub country: CountryCode,
    pub city: String,
}
