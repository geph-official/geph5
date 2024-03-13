use std::{net::SocketAddr, ops::Deref, sync::Arc, time::Duration};

use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use geph5_broker_protocol::{
    BridgeDescriptor, BrokerProtocol, ExitDescriptor, ExitList, GenericError, Mac, RouteDescriptor,
    Signed, DOMAIN_EXIT_DESCRIPTOR,
};
use isocountry::CountryCode;
use moka::future::Cache;
use once_cell::sync::Lazy;

use crate::{
    database::{insert_exit, query_bridges, ExitRow, POSTGRES},
    routes::bridge_to_leaf_route,
    CONFIG_FILE,
};

pub struct BrokerImpl {}

#[async_trait]
impl BrokerProtocol for BrokerImpl {
    async fn get_exits(&self) -> Result<Signed<ExitList>, GenericError> {
        static MASTER_SECRET: Lazy<SigningKey> = Lazy::new(|| {
            SigningKey::from_bytes(
                std::fs::read(&CONFIG_FILE.wait().master_secret)
                    .unwrap()
                    .as_slice()
                    .try_into()
                    .unwrap(),
            )
        });

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

    async fn get_routes(&self, exit: SocketAddr) -> Result<RouteDescriptor, GenericError> {
        // TODO auth
        let raw_descriptors = query_bridges("").await?;
        let mut routes = vec![];
        for desc in raw_descriptors {
            match bridge_to_leaf_route(&desc, exit).await {
                Ok(route) => routes.push(route),
                Err(err) => {
                    tracing::warn!(
                        err = debug(err),
                        bridge = debug(desc),
                        "could not communicate"
                    )
                }
            }
        }
        Ok(RouteDescriptor::Race(routes))
    }

    async fn put_exit(&self, descriptor: Mac<Signed<ExitDescriptor>>) -> Result<(), GenericError> {
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

    async fn put_bridge(&self, descriptor: Mac<BridgeDescriptor>) -> Result<(), GenericError> {
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
}
