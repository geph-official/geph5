use async_trait::async_trait;
use bytes::Bytes;
use cadence::prelude::*;
use cadence::{StatsdClient, UdpMetricSink};
use ed25519_dalek::VerifyingKey;
use futures_util::future::join_all;
use geph5_broker_protocol::{
    AccountLevel, AuthError, BridgeDescriptor, BrokerProtocol, BrokerService, Captcha, Credential,
    ExitDescriptor, ExitList, GenericError, Mac, RouteDescriptor, Signed, DOMAIN_EXIT_DESCRIPTOR,
};
use isocountry::CountryCode;
use mizaru2::{BlindedClientToken, BlindedSignature, ClientToken, UnblindedSignature};
use moka::future::Cache;
use nanorpc::{RpcService, ServerError};
use once_cell::sync::Lazy;
use reqwest::StatusCode;
use std::{
    net::SocketAddr,
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::log_error;
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

    /// Obtains a fresh CAPTCHA for user registration.
    async fn get_captcha(&self) -> Result<Captcha, GenericError> {
        // call out to the microservice
        let captcha_service = "https://single-verve-156821.ew.r.appspot.com";
        let resp = reqwest::get(&format!("{}/new", &captcha_service)).await?;
        let captcha_id;
        if resp.status() == StatusCode::OK {
            captcha_id = String::from_utf8_lossy(&resp.bytes().await?).into();
        } else {
            return Err(GenericError(
                "cannot contact captcha microservice to generate".into(),
            ));
        }

        let resp = reqwest::get(&format!("{}/img/{}", &captcha_service, captcha_id)).await?;
        let png_data;
        if resp.status() == StatusCode::OK {
            png_data = resp.bytes().await?;
        } else {
            return Err(GenericError(
                "cannot concact captcha microservice to render".into(),
            ));
        }
        Ok(Captcha {
            captcha_id,
            png_data,
        })
    }

    async fn verify_captcha(
        &self,
        captcha: Captcha,
        solution: String,
    ) -> Result<bool, GenericError> {
        let captcha_service = "https://single-verve-156821.ew.r.appspot.com";
        // call out to the microservice
        let resp = reqwest::get(&format!(
            "{}/solve?id={}&soln={}",
            captcha_service, captcha.captcha_id, solution
        ))
        .await?;
        // TODO handle network errors
        Ok(resp.status() == StatusCode::OK)
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
        let user_level = match valid_auth_token(&auth_token).await {
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
        static EXIT_CACHE: Lazy<Cache<(), Signed<ExitList>>> = Lazy::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .build()
        });

        EXIT_CACHE
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
                Ok(Signed::new(
                    exit_list,
                    DOMAIN_EXIT_DESCRIPTOR,
                    MASTER_SECRET.deref(),
                ))
            })
            .await
            .map_err(|e: Arc<GenericError>| e.deref().clone())
    }

    async fn get_routes(
        &self,
        token: ClientToken,
        sig: UnblindedSignature,
        exit: SocketAddr,
    ) -> Result<RouteDescriptor, GenericError> {
        // authenticate the token
        let _account_level = if PLUS_MIZARU_SK
            .to_public_key()
            .blind_verify(token, &sig)
            .is_ok()
        {
            AccountLevel::Plus
        } else {
            FREE_MIZARU_SK.to_public_key().blind_verify(token, &sig)?;

            AccountLevel::Free
        };

        // TODO filter out plus only

        let raw_descriptors = query_bridges(&format!("{:?}", token)).await?;
        let mut routes = vec![];
        for route in join_all(
            raw_descriptors
                .into_iter()
                .map(|desc| bridge_to_leaf_route(desc, exit)),
        )
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

static STATSD_CLIENT: Lazy<Option<StatsdClient>> = Lazy::new(|| {
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
