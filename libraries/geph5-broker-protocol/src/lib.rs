use std::{fmt::Display, net::SocketAddr};

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
use serde_with::serde_as;
pub use signed::*;

mod mac;
pub use mac::*;
mod bridge;
pub use bridge::*;
use thiserror::Error;

#[nanorpc_derive]
#[async_trait]
pub trait BrokerProtocol {
    async fn get_mizaru_subkey(&self, level: AccountLevel, epoch: u16) -> Bytes;
    async fn get_auth_token(&self, credential: Credential) -> Result<String, AuthError>;
    /// Obtains a unique captcha, for user registry.
    async fn get_captcha(&self) -> Result<Captcha, GenericError>;
    async fn verify_captcha(
        &self,
        captcha: Captcha,
        solution: String,
    ) -> Result<bool, GenericError>;
    async fn get_connect_token(
        &self,
        auth_token: String,
        level: AccountLevel,
        epoch: u16,
        blind_token: BlindedClientToken,
    ) -> Result<BlindedSignature, AuthError>;

    async fn get_exits(&self) -> Result<Signed<ExitList>, GenericError>;
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
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AccountLevel {
    Free,
    Plus,
}

/// A Captcha
#[serde_as]
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Hash)]
pub struct Captcha {
    pub captcha_id: String,
    #[serde_as(as = "serde_with::base64::Base64")]
    pub png_data: Bytes,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    TestDummy,
    LegacyUsernamePassword { username: String, password: String },
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
