use async_trait::async_trait;
use bytes::Bytes;
use cadence::prelude::*;
use cadence::{StatsdClient, UdpMetricSink};
use ed25519_dalek::VerifyingKey;
use futures_util::{future::join_all, TryFutureExt};
use geph5_broker_protocol::{
    AccountLevel, AuthError, BridgeDescriptor, BrokerProtocol, BrokerService, Credential,
    ExitDescriptor, ExitList, GenericError, Mac, RouteDescriptor, Signed, UserInfo,
    DOMAIN_EXIT_DESCRIPTOR,
};
use isocountry::CountryCode;
use mizaru2::{BlindedClientToken, BlindedSignature, ClientToken, UnblindedSignature};
use moka::future::Cache;
use nanorpc::{RpcService, ServerError};
use once_cell::sync::Lazy;
use std::{
    net::SocketAddr,
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{auth::get_subscription_expiry, log_error};
use crate::{
    auth::{new_auth_token, valid_auth_token, validate_username_pwd},
    database::{insert_exit, query_bridges, ExitRow, POSTGRES},
    routes::bridge_to_leaf_route,
    CONFIG_FILE, FREE_MIZARU_SK, MASTER_SECRET, PLUS_MIZARU_SK,
};

pub struct WrappedBrokerService(BrokerService<BrokerImpl>);

impl WrappedBrokerService {
    pub fn new() -> Self {
        Self(BrokerService(BrokerImpl {}))
    }
}

#[async_trait]
impl RpcService for WrappedBrokerService {
    async fn respond(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
    ) -> Option<Result<serde_json::Value, ServerError>> {
        if let Some(client) = STATSD_CLIENT.as_ref() {
            client.count(&format!("broker.{method}"), 1).unwrap();
        }
        self.0.respond(method, params).await
    }
}

struct BrokerImpl {}

impl BrokerImpl {
    async fn get_all_exits(&self) -> Result<ExitList, GenericError> {
        static EXIT_CACHE: Lazy<Cache<(), ExitList>> = Lazy::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .build()
        });

        let exit_list = EXIT_CACHE
            .try_get_with((), async {
                let exits: Vec<(VerifyingKey, ExitDescriptor)> =
                    sqlx::query_as("select * from exits_new")
                        .fetch_all(POSTGRES.deref())
                        .await?
                        .into_iter()
                        .map(|row: ExitRow| {
                            (
                                VerifyingKey::from_bytes(&row.pubkey).unwrap(),
                                ExitDescriptor {
                                    c2e_listen: row.c2e_listen.parse().unwrap(),
                                    b2e_listen: row.b2e_listen.parse().unwrap(),
                                    country: CountryCode::for_alpha2_caseless(&row.country)
                                        .unwrap(),
                                    city: row.city,
                                    load: row.load,
                                    expiry: row.expiry as _,
                                },
                            )
                        })
                        .collect();
                let exit_list = ExitList {
                    all_exits: exits,
                    city_names: serde_yaml::from_str(include_str!("city_names.yaml")).unwrap(),
                };
                Ok(exit_list)
            })
            .await
            .map_err(|e: Arc<GenericError>| e.deref().clone())?;
        Ok(exit_list)
    }
}

fn is_plus_exit(exit: &ExitDescriptor) -> bool {
    !matches!(
        exit.country,
        CountryCode::CAN | CountryCode::NLD | CountryCode::FRA | CountryCode::POL
    )
}

#[async_trait]
impl BrokerProtocol for BrokerImpl {
    async fn get_mizaru_subkey(&self, level: AccountLevel, epoch: u16) -> Bytes {
        match level {
            AccountLevel::Free => &FREE_MIZARU_SK,
            AccountLevel::Plus => &PLUS_MIZARU_SK,
        }
        .get_subkey(epoch)
        .public_key()
        .unwrap()
        .to_der()
        .unwrap()
        .into()
    }

    async fn get_auth_token(&self, credential: Credential) -> Result<String, AuthError> {
        let user_id = match credential {
            Credential::TestDummy => 42,
            Credential::LegacyUsernamePassword { username, password } => {
                validate_username_pwd(&username, &password).await?
            }
        };

        let token = new_auth_token(user_id)
            .await
            .inspect_err(log_error)
            .map_err(|_| AuthError::RateLimited)?;

        Ok(token)
    }

    async fn get_connect_token(
        &self,
        auth_token: String,
        level: AccountLevel,
        epoch: u16,
        blind_token: BlindedClientToken,
    ) -> Result<BlindedSignature, AuthError> {
        let (_, user_level) = match valid_auth_token(&auth_token).await {
            Ok(auth) => {
                if let Some(level) = auth {
                    level
                } else {
                    return Err(AuthError::Forbidden);
                }
            }
            Err(err) => {
                tracing::warn!(err = debug(err), "database failed");
                return Err(AuthError::RateLimited);
            }
        };
        let start = Instant::now();
        if user_level != level {
            return Err(AuthError::WrongLevel);
        }
        let signed = match level {
            AccountLevel::Free => &FREE_MIZARU_SK,
            AccountLevel::Plus => &PLUS_MIZARU_SK,
        }
        .blind_sign(epoch, &blind_token);
        tracing::debug!(elapsed = debug(start.elapsed()), "blind signing done");
        Ok(signed)
    }

    async fn get_exits(&self) -> Result<Signed<ExitList>, GenericError> {
        let exit_list = self.get_all_exits().await?;

        Ok(Signed::new(
            exit_list,
            DOMAIN_EXIT_DESCRIPTOR,
            MASTER_SECRET.deref(),
        ))
    }

    async fn get_free_exits(&self) -> Result<Signed<ExitList>, GenericError> {
        let mut exit_list = self.get_all_exits().await?;
        exit_list.all_exits.retain(|(_, e)| !is_plus_exit(e));
        Ok(Signed::new(
            exit_list,
            DOMAIN_EXIT_DESCRIPTOR,
            MASTER_SECRET.deref(),
        ))
    }

    async fn get_user_info(&self, auth_token: String) -> Result<Option<UserInfo>, AuthError> {
        static USER_INFO_CACHE: Lazy<Cache<String, Option<UserInfo>>> = Lazy::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .build()
        });

        USER_INFO_CACHE
            .try_get_with(auth_token.clone(), async {
                match valid_auth_token(&auth_token).await {
                    Ok(Some((user_id, _))) => {
                        let plus_expires_unix = get_subscription_expiry(user_id)
                            .await
                            .map_err(|_| AuthError::RateLimited)?
                            .map(|u| u as u64);

                        Ok(Some(UserInfo {
                            user_id: user_id as _,
                            plus_expires_unix,
                        }))
                    }
                    Ok(None) => Ok(None),
                    Err(_) => Err(AuthError::RateLimited),
                }
            })
            .await
            .map_err(|e| e.deref().clone())
    }

    async fn get_routes(
        &self,
        token: ClientToken,
        sig: UnblindedSignature,
        exit: SocketAddr,
    ) -> Result<RouteDescriptor, GenericError> {
        // authenticate the token
        let account_level = if PLUS_MIZARU_SK
            .to_public_key()
            .blind_verify(token, &sig)
            .is_ok()
        {
            AccountLevel::Plus
        } else {
            FREE_MIZARU_SK.to_public_key().blind_verify(token, &sig)?;

            AccountLevel::Free
        };

        let raw_descriptors = query_bridges(&format!("{:?}", token)).await?;

        let plus_pools = [
            "ls_ap_northeast_1",
            "ls_ap_northeast_2",
            "NEW_ls_ap_northeast_1",
            "NEW_ls_ap_northeast_2",
            "yaofan-hk",
            "yaofan-tw",
        ];
        let raw_descriptors = if account_level == AccountLevel::Free {
            raw_descriptors
                .into_iter()
                .filter(|s| !plus_pools.iter().any(|plus_group| &s.pool == plus_group))
                .collect()
        } else {
            raw_descriptors
        };

        let mut routes = vec![];
        for route in join_all(raw_descriptors.into_iter().map(|desc| {
            let bridge = desc.control_listen;
            bridge_to_leaf_route(desc, exit).inspect_err(move |err| {
                tracing::warn!(
                    err = debug(err),
                    bridge = debug(bridge),
                    exit = debug(exit),
                    "failed to call bridge_to_leaf_route"
                )
            })
        }))
        .await
        {
            match route {
                Ok(route) => routes.push(route),
                Err(err) => {
                    tracing::warn!(err = debug(err), "could not communicate")
                }
            }
        }

        Ok(RouteDescriptor::Race(routes))
    }

    async fn insert_exit(
        &self,
        descriptor: Mac<Signed<ExitDescriptor>>,
    ) -> Result<(), GenericError> {
        let descriptor =
            descriptor.verify(blake3::hash(CONFIG_FILE.wait().exit_token.as_bytes()).as_bytes())?;
        let pubkey = descriptor.pubkey;
        let descriptor = descriptor.verify(DOMAIN_EXIT_DESCRIPTOR, |_| true)?;
        let exit = ExitRow {
            pubkey: pubkey.to_bytes(),
            c2e_listen: descriptor.c2e_listen.to_string(),
            b2e_listen: descriptor.b2e_listen.to_string(),
            country: descriptor.country.alpha2().into(),
            city: descriptor.city.clone(),
            load: descriptor.load,
            expiry: descriptor.expiry as _,
        };
        insert_exit(&exit).await?;
        Ok(())
    }

    async fn insert_bridge(&self, descriptor: Mac<BridgeDescriptor>) -> Result<(), GenericError> {
        let descriptor = descriptor
            .verify(blake3::hash(CONFIG_FILE.wait().bridge_token.as_bytes()).as_bytes())?;

        sqlx::query(
            r#"
            INSERT INTO bridges_new (listen, cookie, pool, expiry)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (listen) DO UPDATE
            SET cookie = $2, pool = $3, expiry = $4
            "#,
        )
        .bind(descriptor.control_listen.to_string())
        .bind(descriptor.control_cookie.to_string())
        .bind(descriptor.pool.to_string())
        .bind(descriptor.expiry as i64)
        .execute(&*POSTGRES)
        .await?;
        Ok(())
    }

    async fn incr_stat(&self, stat: String, value: i32) {
        if let Some(client) = STATSD_CLIENT.as_ref() {
            client.count(&stat, value).unwrap();
        }
    }

    async fn set_stat(&self, stat: String, value: f64) {
        if let Some(client) = STATSD_CLIENT.as_ref() {
            client.gauge(&stat, value).unwrap();
        }
    }
}

pub static STATSD_CLIENT: Lazy<Option<StatsdClient>> = Lazy::new(|| {
    if let Some(statsd_addr) = CONFIG_FILE.wait().statsd_addr {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        Some(StatsdClient::from_sink(
            "geph5",
            UdpMetricSink::from(statsd_addr, socket).unwrap(),
        ))
    } else {
        None
    }
});
