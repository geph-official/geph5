use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use cadence::prelude::*;
use cadence::{StatsdClient, UdpMetricSink};
use ed25519_dalek::VerifyingKey;
use futures_util::{future::join_all, TryFutureExt};
use geph5_broker_protocol::{
    AccountLevel, AuthError, AvailabilityData, BridgeDescriptor, BrokerProtocol, BrokerService,
    Credential, ExitCategory, ExitDescriptor, ExitList, ExitMetadata, GenericError, GetRoutesArgs,
    JsonSigned, Mac, NetStatus, NewsItem, RouteDescriptor, StdcodeSigned, UserInfo, VoucherInfo,
    DOMAIN_EXIT_DESCRIPTOR, DOMAIN_NET_STATUS,
};
use geph5_ip_to_asn::ip_to_asn_country;
use influxdb_line_protocol::LineProtocolBuilder;
use isocountry::CountryCode;
use mizaru2::{
    BlindedClientToken, BlindedSignature, ClientToken, SingleBlindedSignature,
    SingleUnblindedSignature, UnblindedSignature,
};
use moka::future::Cache;
use nanorpc::{RpcService, ServerError};
use once_cell::sync::Lazy;

use std::net::Ipv4Addr;
use std::str::FromStr as _;
use std::sync::atomic::AtomicU64;
use std::sync::LazyLock;
use std::{
    net::SocketAddr,
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::auth::validate_secret;
use crate::database::{consume_bw, insert_exit_metadata, ExitRowWithMetadata};
use crate::BW_MIZARU_SK;
use crate::{
    auth::{get_user_info, register_secret, validate_credential},
    free_voucher::{delete_free_voucher, get_free_voucher},
    log_error,
    news::fetch_news,
    payments::{
        payment_sessid, GiftcardWireInfo, PaymentClient, PaymentTransport, StartAliwechatArgs,
        StartStripeArgs,
    },
    puzzle::{new_puzzle, verify_puzzle_solution},
};
use crate::{
    auth::{new_auth_token, valid_auth_token},
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
        let start = Instant::now();
        let resp = self.0.respond(method, params).await?;
        let method = method.to_string();
        smolscale::spawn(async move {
            if let Some(endpoint) = &CONFIG_FILE.wait().influxdb {
                let _ = endpoint
                    .send_line(
                        LineProtocolBuilder::new()
                            .measurement("broker_rpc_calls")
                            .tag("method", &method)
                            .field("latency", start.elapsed().as_secs_f64())
                            .close_line()
                            .build(),
                    )
                    .await;
            }
        })
        .detach();
        Some(resp)
    }
}

struct BrokerImpl {}

impl BrokerImpl {
    async fn net_status_inner(&self) -> Result<NetStatus, GenericError> {
        static CACHE: Lazy<Cache<(), NetStatus>> = Lazy::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .build()
        });

        let ns = CACHE
            .try_get_with((), async {
                let exits: Vec<ExitRowWithMetadata> =
                    sqlx::query_as("select * from exits_new natural left join exit_metadata")
                        .fetch_all(POSTGRES.deref())
                        .await?;
                let exits = exits
                    .into_iter()
                    .map(|row| {
                        let vk = VerifyingKey::from_bytes(&row.pubkey).unwrap();
                        let desc = ExitDescriptor {
                            c2e_listen: row.c2e_listen.parse().unwrap(),
                            b2e_listen: row.b2e_listen.parse().unwrap(),
                            country: CountryCode::for_alpha2_caseless(&row.country).unwrap(),
                            city: row.city,
                            load: row.load,
                            expiry: row.expiry as _,
                        };
                        let meta = row
                            .metadata
                            .map(|j| j.0)
                            .unwrap_or_else(|| default_exit_metadata(&desc.country));
                        (hex::encode(vk.as_bytes()), (vk, desc, meta))
                    })
                    .collect();
                Ok(NetStatus { exits })
            })
            .await
            .map_err(|e: Arc<GenericError>| e.deref().clone())?;
        Ok(ns)
    }
}

fn default_exit_metadata(country: &CountryCode) -> ExitMetadata {
    let mut allowed_levels = vec![AccountLevel::Plus];
    if matches!(
        country,
        CountryCode::CAN
            | CountryCode::NLD
            | CountryCode::FRA
            | CountryCode::POL
            | CountryCode::DEU
    ) {
        allowed_levels.push(AccountLevel::Free);
    }
    ExitMetadata {
        allowed_levels,
        category: ExitCategory::Core,
    }
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
        let user_id = validate_credential(credential).await?;

        let token = new_auth_token(user_id)
            .await
            .inspect_err(log_error)
            .map_err(|_| AuthError::RateLimited)?;

        Ok(token)
    }

    async fn get_bw_token(
        &self,
        auth_token: String,
        blind_token: BlindedClientToken,
    ) -> Result<SingleBlindedSignature, AuthError> {
        let (id, _) = valid_auth_token(auth_token)
            .await
            .map_err(|_| AuthError::RateLimited)?
            .ok_or(AuthError::Forbidden)?;

        consume_bw(id, 10)
            .await
            .map_err(|_| AuthError::RateLimited)?;

        let sig = BW_MIZARU_SK.blind_sign(&blind_token);
        Ok(sig)
    }

    async fn get_connect_token(
        &self,
        auth_token: String,
        level: AccountLevel,
        epoch: u16,
        blind_token: BlindedClientToken,
    ) -> Result<BlindedSignature, AuthError> {
        let (user_id, user_level) = match valid_auth_token(auth_token).await {
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
        static LOGIN_COUNT_CACHE: LazyLock<Cache<String, Arc<AtomicU64>>> = LazyLock::new(|| {
            Cache::builder()
                .time_to_idle(Duration::from_secs(864000))
                .build()
        });
        let counter = LOGIN_COUNT_CACHE
            .get_with(
                format!(
                    "{user_id}-{}",
                    (SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        / 86400)
                ),
                async { Default::default() },
            )
            .await;
        let count = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(user_id, count, "authenticated auth token");
        // exempt special testing account
        if count > 20 && user_id != 9311416 {
            tracing::warn!(user_id, count, "too many auths in the last day, rejecting");

            return Err(AuthError::RateLimited);
        }
        let start = Instant::now();
        // when the user is Plus now, but won't be Plus then, we should return WrongLevel *if the user is claiming to be Plus*.
        if level == AccountLevel::Plus && (user_level != level) {
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

    /// This is a LEGACY endpoint!!
    async fn get_exits(&self) -> Result<StdcodeSigned<ExitList>, GenericError> {
        let ns = self.net_status_inner().await?;
        let all_exits = ns
            .exits
            .into_values()
            .filter(|(_, _, meta)| meta.category == ExitCategory::Core)
            .map(|(vk, desc, _)| (vk, desc))
            .collect();
        let exit_list = ExitList {
            all_exits,
            city_names: serde_yaml::from_str(include_str!("city_names.yaml")).unwrap(),
        };
        Ok(StdcodeSigned::new(
            exit_list,
            DOMAIN_EXIT_DESCRIPTOR,
            MASTER_SECRET.deref(),
        ))
    }

    /// This is a LEGACY endpoint!!
    async fn get_free_exits(&self) -> Result<StdcodeSigned<ExitList>, GenericError> {
        let ns = self.net_status_inner().await?;
        let all_exits = ns
            .exits
            .into_values()
            .filter(|(_, _, meta)| {
                meta.allowed_levels.contains(&AccountLevel::Free)
                    && meta.category == ExitCategory::Core
            })
            .map(|(vk, desc, _)| (vk, desc))
            .collect();
        let exit_list = ExitList {
            all_exits,
            city_names: serde_yaml::from_str(include_str!("city_names.yaml")).unwrap(),
        };
        Ok(StdcodeSigned::new(
            exit_list,
            DOMAIN_EXIT_DESCRIPTOR,
            MASTER_SECRET.deref(),
        ))
    }

    async fn get_user_info(&self, auth_token: String) -> Result<Option<UserInfo>, AuthError> {
        match valid_auth_token(auth_token).await {
            Ok(Some((user_id, _))) => get_user_info(user_id).await,
            Ok(None) => Ok(None),
            Err(_) => Err(AuthError::RateLimited),
        }
    }

    async fn get_user_info_by_cred(&self, cred: Credential) -> Result<Option<UserInfo>, AuthError> {
        let user_id = validate_credential(cred).await;
        if let Err(AuthError::Forbidden) = user_id {
            return Ok(None);
        }
        get_user_info(user_id?).await
    }

    async fn get_routes_v2(&self, args: GetRoutesArgs) -> Result<RouteDescriptor, GenericError> {
        // authenticate the token
        let account_level = if PLUS_MIZARU_SK
            .to_public_key()
            .blind_verify(args.token, &args.sig)
            .is_ok()
        {
            AccountLevel::Plus
        } else {
            FREE_MIZARU_SK
                .to_public_key()
                .blind_verify(args.token, &args.sig)?;

            AccountLevel::Free
        };

        // get the exit
        let exit = self
            .net_status_inner()
            .await?
            .exits
            .into_values()
            .map(|(_, d, _)| d)
            .find(|exit| exit.b2e_listen == args.exit_b2e)
            .context("cannot find this exit")?;

        // for known good countries, we return a direct route!
        let mut direct_route = None;
        let country = if let Some(ip_addr) = args.client_metadata["ip_addr"]
            .as_str()
            .and_then(|ip_addr| Ipv4Addr::from_str(ip_addr).ok())
        {
            let (asn, country) = ip_to_asn_country(ip_addr).await?;
            tracing::debug!(
                asn,
                country = display(&country),
                "obtaining route with metadata"
            );
            if country != "TM" && country != "IR" && country != "RU" && country != "CN" {
                // return a DIRECT route!
                direct_route = Some(RouteDescriptor::ConnTest {
                    ping_count: 1,
                    lower: RouteDescriptor::Tcp(exit.c2e_listen).into(),
                });
            }
            country
        } else {
            "".to_string()
        };

        let raw_descriptors = query_bridges(&format!("{:?}", args.token)).await?;

        let raw_descriptors = if account_level == AccountLevel::Free {
            raw_descriptors
                .into_iter()
                .filter(|(_, _, is_plus)| !is_plus)
                .collect()
        } else {
            raw_descriptors
        };

        let raw_descriptors = raw_descriptors.into_iter().filter(|desc| {
            // for China Plus users, filter out ovh
            if account_level == AccountLevel::Plus && country == "CN" {
                return !desc.0.pool.contains("ovh");
            }
            // TM-only bridges
            if desc.0.pool.contains("TM") {
                return country == "TM";
            }
            true
        });

        let mut routes = vec![];
        for route in (join_all(raw_descriptors.into_iter().map(|(desc, delay_ms, _)| {
            let bridge = desc.control_listen;
            bridge_to_leaf_route(desc, delay_ms, &exit, &country).inspect_err(move |err| {
                tracing::warn!(
                    err = debug(err),
                    bridge = debug(bridge),
                    exit = debug(args.exit_b2e),
                    "failed to call bridge_to_leaf_route"
                )
            })
        }))
        .await)
            .into_iter()
            .flatten()
        {
            routes.push(route)
        }

        Ok(if let Some(route) = direct_route {
            RouteDescriptor::Race(vec![
                route,
                RouteDescriptor::Delay {
                    milliseconds: 2000,
                    lower: RouteDescriptor::Race(routes).into(),
                },
            ])
        } else {
            RouteDescriptor::Race(routes)
        })
    }

    async fn get_routes(
        &self,
        token: ClientToken,
        sig: UnblindedSignature,
        exit_b2e: SocketAddr,
    ) -> Result<RouteDescriptor, GenericError> {
        self.get_routes_v2(GetRoutesArgs {
            token,
            sig,
            exit_b2e,
            client_metadata: Default::default(),
        })
        .await
    }

    async fn consume_bw_token(
        &self,
        token: ClientToken,
        sig: SingleUnblindedSignature,
    ) -> Result<(), AuthError> {
        BW_MIZARU_SK
            .to_public_key()
            .blind_verify(token, &sig)
            .map_err(|_| AuthError::Forbidden)?;
        // TODO prevent replays by writing to a DB. This requires something smarter than stuffing it into postgresql and causing a neverending log.
        Ok(())
    }

    async fn insert_exit(
        &self,
        descriptor: Mac<StdcodeSigned<ExitDescriptor>>,
    ) -> Result<(), GenericError> {
        let descriptor =
            descriptor.verify(blake3::hash(CONFIG_FILE.wait().exit_token.as_bytes()).as_bytes())?;
        let pubkey = descriptor.pubkey;
        let descriptor = descriptor.verify(DOMAIN_EXIT_DESCRIPTOR, |_| true)?;

        // Validate that the timestamp is reasonably current
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if descriptor.expiry < now {
            return Err(GenericError(
                "Exit info timestamp is before current time (potential replay attack)".to_string(),
            ));
        }

        let exit = ExitRow {
            pubkey: pubkey.to_bytes(),
            c2e_listen: descriptor.c2e_listen.to_string(),
            b2e_listen: descriptor.b2e_listen.to_string(),
            country: descriptor.country.alpha2().into(),
            city: descriptor.city.clone(),
            load: descriptor.load,
            expiry: (now + 60) as _,
        };
        insert_exit(&exit).await?;
        Ok(())
    }

    async fn insert_exit_v2(
        &self,
        descriptor: Mac<JsonSigned<(ExitDescriptor, ExitMetadata)>>,
    ) -> Result<(), GenericError> {
        let descriptor =
            descriptor.verify(blake3::hash(CONFIG_FILE.wait().exit_token.as_bytes()).as_bytes())?;
        let pubkey = descriptor.pubkey();
        let (descriptor, metadata) = descriptor.verify(DOMAIN_EXIT_DESCRIPTOR, |_| true)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if descriptor.expiry < now {
            return Err(GenericError(
                "Exit info timestamp is before current time (potential replay attack)".to_string(),
            ));
        }

        let exit = ExitRow {
            pubkey: pubkey.to_bytes(),
            c2e_listen: descriptor.c2e_listen.to_string(),
            b2e_listen: descriptor.b2e_listen.to_string(),
            country: descriptor.country.alpha2().into(),
            city: descriptor.city.clone(),
            load: descriptor.load,
            expiry: (now + 60) as _,
        };
        insert_exit(&exit).await?;
        insert_exit_metadata(pubkey.to_bytes(), metadata).await?;
        Ok(())
    }

    async fn get_net_status(&self) -> Result<JsonSigned<NetStatus>, GenericError> {
        let ns = self.net_status_inner().await?;
        Ok(JsonSigned::new(
            ns,
            DOMAIN_NET_STATUS,
            MASTER_SECRET.deref(),
        ))
    }

    async fn insert_bridge(&self, descriptor: Mac<BridgeDescriptor>) -> Result<(), GenericError> {
        let descriptor = descriptor
            .verify(blake3::hash(CONFIG_FILE.wait().bridge_token.as_bytes()).as_bytes())?;
        tracing::debug!("inserting bridge from pool {}", descriptor.pool);
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

    async fn upload_available(&self, data: AvailabilityData) {
        smolscale::spawn(
            async move {
                let current_timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
                let mut txn = POSTGRES.begin().await?;
                let up_time: Option<(i64,)> = sqlx::query_as("select last_update from bridge_availability where listen = $1 and user_country = $2 and user_asn = $3").bind(&data.listen).bind(&data.country).bind(&data.asn).fetch_optional(&mut *txn).await?;
                if let Some((up_time,)) = up_time {
                    let diff = current_timestamp.saturating_sub(up_time) as f64;
                    // 1-hour decay interval
                    let decay_factor = 2.0f64.powf(diff / 3600.0);
                    if data.success {
                        sqlx::query("update bridge_availability set successes = successes / $1 + 1, last_update = $2 where listen = $3 and user_country = $4 and user_asn = $5").bind(decay_factor).bind(current_timestamp).bind(&data.listen).bind(&data.country).bind(&data.asn).execute(&mut *txn).await?;
                    } else {
                        sqlx::query("update bridge_availability set failures = failures / $1 + 1, last_update = $2 where listen = $3 and user_country = $4 and user_asn = $5").bind(decay_factor).bind(current_timestamp).bind(&data.listen).bind(&data.country).bind(&data.asn).execute(&mut *txn).await?;
                    }
                } else if data.success {
                    sqlx::query("insert into bridge_availability (listen, user_country, user_asn, successes, failures, last_update) values ($1, $2, $3, 1.0, 0.0, $4)").bind(&data.listen).bind(&data.country).bind(&data.asn).bind(current_timestamp).execute(&mut *txn).await?;
                } else {
                    sqlx::query("insert into bridge_availability (listen, user_country, user_asn, successes, failures, last_update) values ($1, $2, $3, 0.0, 1.0, $4)").bind(&data.listen).bind(&data.country).bind(&data.asn).bind(current_timestamp).execute(&mut *txn).await?;
                }
                txn.commit().await?;
                anyhow::Ok(())
            }
            .inspect_err(|e| tracing::warn!(err = debug(e), "setting availability failed")),
        )
        .detach();
    }

    async fn get_puzzle(&self) -> (String, u16) {
        (new_puzzle().await, CONFIG_FILE.wait().puzzle_difficulty)
    }

    async fn register_user_secret(
        &self,
        puzzle: String,
        solution: String,
    ) -> Result<String, GenericError> {
        verify_puzzle_solution(&puzzle, &solution).await?;
        Ok(register_secret(None).await?)
    }

    async fn upgrade_to_secret(&self, cred: Credential) -> Result<String, AuthError> {
        let user_id = validate_credential(cred).await?;
        register_secret(Some(user_id))
            .map_err(|_| AuthError::RateLimited)
            .await
    }

    async fn delete_account(&self, secret: String) -> Result<(), GenericError> {
        // validate secret; get user_id
        let user_id = validate_secret(&secret).await?;
        // cancel stripe
        let rpc = PaymentClient(PaymentTransport);
        let sessid = payment_sessid(user_id).await?;
        rpc.cancel_recurring(sessid)
            .await?
            .map_err(|e| GenericError(e))?;
        // delete for good
        sqlx::query("delete from users where id=(select id from auth_secret where secret=$1)")
            .bind(secret)
            .execute(POSTGRES.deref())
            .await?;
        Ok(())
    }

    async fn get_news(&self, lang: String) -> Result<Vec<NewsItem>, GenericError> {
        let (send, recv) = oneshot::channel();
        smolscale::spawn(async move { send.send(fetch_news(&lang).await) }).detach();
        recv.await.unwrap().map_err(|e: anyhow::Error| e.into())
    }

    async fn raw_price_points(&self) -> Result<Vec<(u32, u32)>, GenericError> {
        Ok(vec![(30, 500), (90, 1500), (365, 5475), (730, 10342)])
    }

    async fn payment_methods(&self) -> Result<Vec<String>, GenericError> {
        Ok(vec![
            "credit-card".into(),
            "wechat".to_string(),
            // "alipay".to_string(),
        ])
    }

    async fn create_payment(
        &self,
        secret: String,
        days: u32,
        method: String,
    ) -> Result<String, GenericError> {
        let user_id = validate_credential(Credential::Secret(secret.clone())).await?;
        let rpc = PaymentClient(PaymentTransport);
        let sessid = payment_sessid(user_id).await?;
        match method.as_str() {
            "credit-card" => Ok(rpc
                .start_stripe_url(
                    sessid,
                    StartStripeArgs {
                        promo: "".to_string(),
                        days: days as _,
                        item: crate::payments::Item::Plus,
                        is_recurring: true,
                    },
                )
                .await?
                .map_err(GenericError)?),
            "wechat" => Ok(rpc
                .start_aliwechat(
                    sessid,
                    StartAliwechatArgs {
                        promo: "".to_string(),
                        days: days as _,
                        item: crate::payments::Item::Plus,
                        method: "wxpay".to_string(),
                        mobile: false,
                    },
                )
                .await?
                .map_err(GenericError)?),
            "alipay" => Ok(rpc
                .start_aliwechat(
                    sessid,
                    StartAliwechatArgs {
                        promo: "".to_string(),
                        days: days as _,
                        item: crate::payments::Item::Plus,
                        method: "alipay".to_string(),
                        mobile: false,
                    },
                )
                .await?
                .map_err(GenericError)?),
            _ => Err(GenericError("no support for this method here".to_string())),
        }
    }

    async fn get_free_voucher(&self, secret: String) -> Result<Option<VoucherInfo>, GenericError> {
        // TODO a db-driven implementation
        let user_id = validate_credential(Credential::Secret(secret)).await?;
        // if user_id == 42 {
        let info = get_free_voucher(user_id).await?;
        Ok(info)
        // } else {
        //     Ok(None)
        // }
    }

    async fn redeem_voucher(&self, secret: String, code: String) -> Result<i32, GenericError> {
        // Validate the secret and get the user ID
        let user_id = validate_credential(Credential::Secret(secret)).await?;

        // Get a payment session for the user
        let sessid = payment_sessid(user_id).await?;

        // Delete the free voucher after successful redemption
        delete_free_voucher(user_id).await?;

        // Call the payment service to spend the gift card
        let days = PaymentClient(PaymentTransport)
            .spend_giftcard(
                sessid,
                GiftcardWireInfo {
                    gc_id: code,
                    promo: "".to_string(),
                },
            )
            .await?
            .map_err(|e| GenericError(format!("Failed to redeem voucher: {}", e)))?;

        // Return the number of days credited to the account
        Ok(days)
    }

    async fn upload_debug_pack(
        &self,
        email: Option<String>,
        logs: String,
    ) -> Result<(), GenericError> {
        // Dummy implementation - in real implementation, this would store the logs and email
        tracing::info!(
            email = debug(email),
            logs_len = logs.len(),
            "Debug pack uploaded"
        );

        // In a real implementation we would:
        // 1. Store the logs in a database or file system
        // 2. If email is provided, notify support personnel
        // 3. Generate a reference ID for the debug pack

        Ok(())
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
