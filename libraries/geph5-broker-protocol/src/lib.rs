use std::{collections::BTreeMap, fmt::Display, net::SocketAddr};

use async_trait::async_trait;
use bytes::Bytes;
use mizaru2::{BlindedClientToken, BlindedSignature, ClientToken, UnblindedSignature};
use nanorpc::nanorpc_derive;
mod route;
pub use route::*;
mod exit;
pub use exit::*;
mod signed;
use serde::{Deserialize, Serialize};
pub use signed::*;
mod mac;
pub mod puzzle;
pub use mac::*;
mod bridge;
pub use bridge::*;
use thiserror::Error;

#[nanorpc_derive]
#[async_trait]
pub trait BrokerProtocol {
    async fn get_mizaru_subkey(&self, level: AccountLevel, epoch: u16) -> Bytes;
    async fn get_auth_token(&self, credential: Credential) -> Result<String, AuthError>;
    async fn get_user_info(&self, auth_token: String) -> Result<Option<UserInfo>, AuthError>;
    async fn get_user_info_by_cred(
        &self,
        credential: Credential,
    ) -> Result<Option<UserInfo>, AuthError>;
    async fn get_connect_token(
        &self,
        auth_token: String,
        level: AccountLevel,
        epoch: u16,
        blind_token: BlindedClientToken,
    ) -> Result<BlindedSignature, AuthError>;

    async fn get_exits(&self) -> Result<Signed<ExitList>, GenericError>;
    async fn get_free_exits(&self) -> Result<Signed<ExitList>, GenericError>;
    async fn get_routes(
        &self,
        token: ClientToken,
        sig: UnblindedSignature,
        exit_b2e: SocketAddr,
    ) -> Result<RouteDescriptor, GenericError>;

    async fn insert_exit(
        &self,
        descriptor: Mac<Signed<ExitDescriptor>>,
    ) -> Result<(), GenericError>;

    async fn insert_bridge(&self, descriptor: Mac<BridgeDescriptor>) -> Result<(), GenericError>;

    async fn incr_stat(&self, stat: String, value: i32);

    async fn set_stat(&self, stat: String, value: f64);

    async fn upload_available(&self, data: AvailabilityData);

    async fn get_puzzle(&self) -> (String, u16);

    async fn register_user_secret(
        &self,
        puzzle: String,
        solution: String,
    ) -> Result<String, GenericError>;
    async fn upgrade_to_secret(&self, cred: Credential) -> Result<String, AuthError>;

    async fn get_news(&self, lang: String) -> Result<Vec<NewsItem>, GenericError>;

    async fn raw_price_points(&self) -> Result<Vec<(u32, u32)>, GenericError>;
    async fn payment_methods(&self) -> Result<Vec<String>, GenericError>;
    async fn create_payment(
        &self,
        auth_token: String,
        days: u32,
        method: String,
    ) -> Result<String, GenericError>;

    async fn get_free_voucher(&self, secret: String) -> Result<Option<VoucherInfo>, GenericError>;

    // Redeem a voucher/gift card code to add credit to the user's account
    async fn redeem_voucher(&self, secret: String, code: String) -> Result<i32, GenericError>;

    // Upload debug information for troubleshooting purposes
    async fn upload_debug_pack(
        &self,
        email: Option<String>,
        logs: String,
    ) -> Result<(), GenericError>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VoucherInfo {
    pub code: String,
    pub explanation: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailabilityData {
    pub listen: String,
    pub country: String,
    pub asn: String,
    pub success: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInfo {
    pub user_id: u64,
    pub plus_expires_unix: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AccountLevel {
    Free,
    Plus,
}

#[derive(Clone, Debug, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthError {
    #[error("rate limited")]
    RateLimited,
    #[error("incorrect credentials")]
    Forbidden,
    #[error("wrong level")]
    WrongLevel,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    TestDummy,
    LegacyUsernamePassword { username: String, password: String },
    Secret(String),
}

impl Default for Credential {
    fn default() -> Self {
        Self::TestDummy
    }
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NewsItem {
    pub title: String,
    pub date_unix: u64,
    pub contents: String,
}
