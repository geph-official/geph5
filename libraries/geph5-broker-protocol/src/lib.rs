use std::{collections::BTreeMap, fmt::Display, net::SocketAddr};

use async_trait::async_trait;
use bytes::Bytes;
use mizaru2::{
    BlindedClientToken, BlindedSignature, ClientToken, SingleBlindedSignature,
    SingleUnblindedSignature, UnblindedSignature,
};
use nanorpc::{JrpcRequest, JrpcResponse, nanorpc_derive};
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
    async fn opaque_abtest(&self, test: String, id: u64) -> bool;

    async fn call_geph_payments(&self, jrpc_req: JrpcRequest)
    -> Result<JrpcResponse, GenericError>;

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

    async fn get_bw_token(
        &self,
        auth_token: String,
        blind_token: BlindedClientToken,
    ) -> Result<SingleBlindedSignature, AuthError>;

    async fn consume_bw_token(
        &self,
        token: ClientToken,
        sig: SingleUnblindedSignature,
    ) -> Result<(), AuthError>;

    async fn get_exits(&self) -> Result<StdcodeSigned<ExitList>, GenericError>;
    async fn get_free_exits(&self) -> Result<StdcodeSigned<ExitList>, GenericError>;

    /// Gets the network status. This is the newer endpoint that clients should use.
    async fn get_net_status(&self) -> Result<JsonSigned<NetStatus>, GenericError>;
    async fn get_exit_route(
        &self,
        args: GetExitRouteArgs,
    ) -> Result<JsonSigned<ExitRouteDescriptor>, GenericError>;

    async fn get_routes(
        &self,
        token: ClientToken,
        sig: UnblindedSignature,
        exit_b2e: SocketAddr,
    ) -> Result<RouteDescriptor, GenericError>;
    async fn get_routes_v2(&self, args: GetRoutesArgs) -> Result<RouteDescriptor, GenericError>;

    async fn insert_exit(
        &self,
        descriptor: Mac<StdcodeSigned<ExitDescriptor>>,
    ) -> Result<(), GenericError>;

    async fn insert_exit_v2(
        &self,
        descriptor: Mac<JsonSigned<(ExitDescriptor, ExitMetadata)>>,
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

    async fn delete_account(&self, secret: String) -> Result<(), GenericError>;

    async fn get_news(&self, lang: String) -> Result<Vec<LegacyNewsItem>, GenericError>;

    async fn raw_price_points(&self) -> Result<Vec<(u32, u32)>, GenericError>;
    async fn basic_price_points(&self) -> Result<Vec<(u32, u32)>, GenericError>;
    async fn basic_mb_limit(&self) -> u32;

    async fn payment_methods(&self) -> Result<Vec<String>, GenericError>;

    async fn create_payment(
        &self,
        auth_token: String,
        days: u32,
        method: String,
    ) -> Result<String, GenericError>;

    async fn create_basic_payment(
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
pub struct GetRoutesArgs {
    pub token: ClientToken,
    pub sig: UnblindedSignature,
    pub exit_b2e: SocketAddr,
    pub client_metadata: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetExitRouteArgs {
    pub token: ClientToken,
    pub sig: UnblindedSignature,
    pub exit_constraint: ExitConstraint,
    pub client_metadata: serde_json::Value,
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
    #[serde(default)]
    pub recurring: bool,
    #[serde(default)]
    pub bw_consumption: Option<BwConsumptionInfo>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BwConsumptionInfo {
    pub mb_used: u32,
    pub mb_limit: u32,
    pub renew_unix: u64,
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
#[derive(Default)]
pub enum Credential {
    #[default]
    TestDummy,
    LegacyUsernamePassword {
        username: String,
        password: String,
    },
    Secret(String),
}

pub const DOMAIN_EXIT_DESCRIPTOR: &str = "exit-descriptor";

pub const DOMAIN_NET_STATUS: &str = "net-status";

pub const DOMAIN_EXIT_ROUTE: &str = "exit-route";

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
pub struct LegacyNewsItem {
    pub title: String,
    pub date_unix: u64,
    pub contents: String,
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use isocountry::CountryCode;

    use super::*;

    #[test]
    fn exit_route_roundtrip_and_verify() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let descriptor = ExitRouteDescriptor {
            exit_pubkey: signing_key.verifying_key(),
            exit: ExitDescriptor {
                c2e_listen: "127.0.0.1:9000".parse().unwrap(),
                b2e_listen: "127.0.0.1:9001".parse().unwrap(),
                country: CountryCode::CAN,
                city: "Toronto".into(),
                load: 0.25,
                expiry: 123,
            },
            route: RouteDescriptor::Tcp("127.0.0.1:9002".parse().unwrap()),
        };
        let signed = JsonSigned::new(descriptor.clone(), DOMAIN_EXIT_ROUTE, &signing_key);
        let encoded = serde_json::to_string(&signed).unwrap();
        let decoded: JsonSigned<ExitRouteDescriptor> = serde_json::from_str(&encoded).unwrap();
        let verified = decoded
            .verify(DOMAIN_EXIT_ROUTE, |pk| *pk == signing_key.verifying_key())
            .unwrap();
        assert_eq!(verified.exit_pubkey, descriptor.exit_pubkey);
        assert_eq!(verified.exit.c2e_listen, descriptor.exit.c2e_listen);
        match verified.route {
            RouteDescriptor::Tcp(addr) => assert_eq!(addr, "127.0.0.1:9002".parse().unwrap()),
            _ => panic!("unexpected route variant"),
        }
    }

    #[test]
    fn exit_route_verify_rejects_wrong_domain() {
        let signing_key = SigningKey::from_bytes(&[8; 32]);
        let signed = JsonSigned::new(
            ExitRouteDescriptor {
                exit_pubkey: signing_key.verifying_key(),
                exit: ExitDescriptor {
                    c2e_listen: "127.0.0.1:9000".parse().unwrap(),
                    b2e_listen: "127.0.0.1:9001".parse().unwrap(),
                    country: CountryCode::CAN,
                    city: "Toronto".into(),
                    load: 0.25,
                    expiry: 123,
                },
                route: RouteDescriptor::Tcp("127.0.0.1:9002".parse().unwrap()),
            },
            DOMAIN_EXIT_ROUTE,
            &signing_key,
        );

        assert!(
            signed
                .verify(DOMAIN_NET_STATUS, |pk| *pk == signing_key.verifying_key())
                .is_err()
        );
    }
}
