use std::time::SystemTime;

use async_trait::async_trait;
use geph5_broker_protocol::{
    AccountLevel, BwConsumptionInfo, ExitDescriptor, NetStatus, VoucherInfo,
};
use nanorpc::nanorpc_derive;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "state")]
pub enum ConnInfo {
    Disconnected,
    Connecting,
    Connected(ConnectedInfo),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConnectedInfo {
    pub protocol: String,
    pub bridge: String,

    pub exit: ExitDescriptor,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ControlUserInfo {
    pub user_id: u64,
    pub level: AccountLevel,

    pub recurring: bool,
    pub expiry: Option<u64>,

    pub bw_consumption: Option<BwConsumptionInfo>,
}

#[nanorpc_derive]
#[async_trait]
pub trait ControlProtocol {
    async fn ab_test(&self, key: String, secret: String) -> Result<bool, String>;
    async fn conn_info(&self) -> ConnInfo;
    async fn stat_num(&self, stat: String) -> f64;
    async fn start_time(&self) -> SystemTime;
    async fn stop(&self);

    async fn recent_logs(&self) -> Vec<String>;

    // broker-proxying stuff

    async fn check_secret(&self, secret: String) -> Result<bool, String>;
    async fn user_info(&self, secret: String) -> Result<ControlUserInfo, String>;
    async fn start_registration(&self) -> Result<usize, String>;
    async fn delete_account(&self, secret: String) -> Result<(), String>;
    async fn poll_registration(&self, idx: usize) -> Result<RegistrationProgress, String>;
    async fn convert_legacy_account(
        &self,
        username: String,
        password: String,
    ) -> Result<String, String>;
    async fn stat_history(&self, stat: String) -> Result<Vec<f64>, String>;

    async fn net_status(&self) -> Result<NetStatus, String>;
    async fn latest_news(&self, lang: String) -> Result<Vec<NewsItem>, String>;
    async fn price_points(&self) -> Result<Vec<(u32, f64)>, String>;
    async fn basic_price_points(&self) -> Result<Vec<(u32, f64)>, String>;
    async fn basic_mb_limit(&self) -> Result<u32, String>;
    async fn payment_methods(&self) -> Result<Vec<String>, String>;
    async fn create_payment(
        &self,
        secret: String,
        days: u32,
        method: String,
    ) -> Result<String, String>;
    async fn create_basic_payment(
        &self,
        secret: String,
        days: u32,
        method: String,
    ) -> Result<String, String>;
    async fn get_free_voucher(&self, secret: String) -> Result<Option<VoucherInfo>, String>;
    async fn redeem_voucher(&self, secret: String, code: String) -> Result<i32, String>;
    async fn export_debug_pack(
        &self,
        email: Option<String>,
        contents: String,
    ) -> Result<(), String>;

    async fn get_update_manifest(&self) -> Result<(serde_json::Value, String), String>;
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RegistrationProgress {
    pub progress: f64,
    pub secret: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NewsItem {
    pub title: String,
    pub date_unix: u64,
    pub contents: String,
    pub important: bool,
}
