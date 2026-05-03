use anyctx::AnyCtx;
use geph5_broker_protocol::{ExitConstraint, ExitRouteDescriptor};
use sqlx::Row;

use crate::{client::Config, database::DATABASE};

async fn ensure_exit_route_cache_table(ctx: &AnyCtx<Config>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS exit_route_cache (
            exit_constraint TEXT PRIMARY KEY,
            route TEXT NOT NULL
        );",
    )
    .execute(ctx.get(DATABASE))
    .await?;
    Ok(())
}

fn serialize_exit_constraint(exit_constraint: &ExitConstraint) -> anyhow::Result<String> {
    Ok(serde_json::to_string(exit_constraint)?)
}

pub async fn write_cached_exit_route(
    ctx: &AnyCtx<Config>,
    exit_constraint: &ExitConstraint,
    route: &ExitRouteDescriptor,
) -> anyhow::Result<()> {
    ensure_exit_route_cache_table(ctx).await?;
    sqlx::query(
        "INSERT INTO exit_route_cache (exit_constraint, route) VALUES (?, ?)
         ON CONFLICT(exit_constraint) DO UPDATE SET route = excluded.route",
    )
    .bind(serialize_exit_constraint(exit_constraint)?)
    .bind(serde_json::to_string(route)?)
    .execute(ctx.get(DATABASE))
    .await?;
    Ok(())
}

pub async fn read_cached_exit_route(
    ctx: &AnyCtx<Config>,
    exit_constraint: &ExitConstraint,
) -> anyhow::Result<Option<ExitRouteDescriptor>> {
    ensure_exit_route_cache_table(ctx).await?;
    let cache_key = serialize_exit_constraint(exit_constraint)?;
    let cached = sqlx::query("SELECT route FROM exit_route_cache WHERE exit_constraint = ?")
        .bind(&cache_key)
        .fetch_optional(ctx.get(DATABASE))
        .await?;
    let Some(row) = cached else {
        return Ok(None);
    };

    let raw_route: String = row.get("route");
    match serde_json::from_str(&raw_route) {
        Ok(route) => Ok(Some(route)),
        Err(err) => {
            tracing::warn!(
                err = debug(&err),
                exit_constraint = %cache_key,
                "cached exit route was corrupt, deleting row"
            );
            sqlx::query("DELETE FROM exit_route_cache WHERE exit_constraint = ?")
                .bind(cache_key)
                .execute(ctx.get(DATABASE))
                .await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{read_cached_exit_route, write_cached_exit_route};
    use crate::client::Config;
    use anyctx::AnyCtx;
    use ed25519_dalek::SigningKey;
    use geph5_broker_protocol::{
        ExitConstraint, ExitDescriptor, ExitRouteDescriptor, RouteDescriptor,
    };
    use isocountry::CountryCode;

    fn test_config() -> Config {
        Config {
            socks5_listen: None,
            http_proxy_listen: None,
            pac_listen: None,
            control_listen: None,
            control_listen_unix: None,
            exit_constraint: ExitConstraint::Auto,
            allow_direct: false,
            cache: Some(temp_db_path()),
            broker: None,
            tunneled_broker: None,
            broker_keys: None,
            port_forward: vec![],
            vpn: false,
            vpn_fd: None,
            spoof_dns: false,
            passthrough_china: false,
            dry_run: true,
            credentials: Default::default(),
            sess_metadata: serde_json::Value::Null,
            task_limit: None,
        }
    }

    fn temp_db_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "geph5-route-cache-test-{}.db",
            rand::random::<u64>()
        ))
    }

    fn sample_route(port: u16) -> ExitRouteDescriptor {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        ExitRouteDescriptor {
            exit_pubkey: signing_key.verifying_key(),
            exit: ExitDescriptor {
                c2e_listen: format!("127.0.0.1:{port}").parse().unwrap(),
                b2e_listen: format!("127.0.0.1:{}", port + 1).parse().unwrap(),
                country: CountryCode::CAN,
                city: "Toronto".into(),
                load: 0.1,
                expiry: 1,
            },
            route: RouteDescriptor::Tcp(format!("127.0.0.1:{}", port + 2).parse().unwrap()),
        }
    }

    #[test]
    fn cached_exit_route_round_trips_and_overwrites() {
        smolscale::block_on(async {
            let ctx = AnyCtx::new(test_config());
            let constraint = ExitConstraint::Country(CountryCode::CAN);
            let initial = sample_route(9000);
            write_cached_exit_route(&ctx, &constraint, &initial)
                .await
                .unwrap();

            let cached = read_cached_exit_route(&ctx, &constraint).await.unwrap();
            assert_eq!(
                cached.unwrap().exit.c2e_listen,
                "127.0.0.1:9000".parse().unwrap()
            );

            let updated = sample_route(9100);
            write_cached_exit_route(&ctx, &constraint, &updated)
                .await
                .unwrap();

            let cached = read_cached_exit_route(&ctx, &constraint).await.unwrap();
            assert_eq!(
                cached.unwrap().exit.c2e_listen,
                "127.0.0.1:9100".parse().unwrap()
            );
        });
    }

    #[test]
    fn corrupt_cached_exit_route_is_deleted() {
        smolscale::block_on(async {
            let ctx = AnyCtx::new(test_config());
            let constraint = ExitConstraint::Hostname("example.com".into());
            let cache_key = serde_json::to_string(&constraint).unwrap();
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS exit_route_cache (
                    exit_constraint TEXT PRIMARY KEY,
                    route TEXT NOT NULL
                );",
            )
            .execute(ctx.get(crate::database::DATABASE))
            .await
            .unwrap();
            sqlx::query("INSERT INTO exit_route_cache (exit_constraint, route) VALUES (?, ?)")
                .bind(&cache_key)
                .bind("{not-json")
                .execute(ctx.get(crate::database::DATABASE))
                .await
                .unwrap();

            assert!(
                read_cached_exit_route(&ctx, &constraint)
                    .await
                    .unwrap()
                    .is_none()
            );

            let remaining: Option<String> =
                sqlx::query_scalar("SELECT route FROM exit_route_cache WHERE exit_constraint = ?")
                    .bind(cache_key)
                    .fetch_optional(ctx.get(crate::database::DATABASE))
                    .await
                    .unwrap();
            assert!(remaining.is_none());
        });
    }
}
