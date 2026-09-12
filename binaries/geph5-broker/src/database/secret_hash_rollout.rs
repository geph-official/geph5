use anyhow::{Context, ensure};
use sqlx::{Connection, PgConnection, PgPool};

const CREATE: &str = include_str!("../../sql/auth_secret_hash_01_create.sql");
const BACKFILL: &str = include_str!("../../sql/auth_secret_hash_02_backfill.sql");
// Session lock spans the two separately committed SQL files. It only coordinates
// new brokers; old account writers must be stopped for the cutover.
const ROLLOUT_LOCK: i64 = 0x6765706853686132;

pub async fn run(pool: &PgPool) -> anyhow::Result<()> {
    // Never return a session carrying an advisory lock or an aborted script
    // transaction to the application pool. Drop also closes it on cancellation.
    let mut connection = pool.acquire().await?.detach();
    let result = run_on_connection(&mut connection).await;
    let closed = connection.close().await;
    result?;
    closed.context("Closing account-secret rollout connection")?;
    Ok(())
}

async fn run_on_connection(connection: &mut PgConnection) -> anyhow::Result<()> {
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(ROLLOUT_LOCK)
        .fetch_one(&mut *connection)
        .await?;
    ensure!(
        locked,
        "Another broker is running the account-secret rollout; retry startup after it finishes"
    );

    let (plaintext, hashes): (bool, bool) = sqlx::query_as(
        "SELECT to_regclass('public.auth_secret') IS NOT NULL,
                to_regclass('public.auth_secret_hash') IS NOT NULL",
    )
    .fetch_one(&mut *connection)
    .await?;
    ensure!(
        plaintext || hashes,
        "Neither auth_secret nor auth_secret_hash exists; check the broker database configuration"
    );

    if plaintext {
        if !hashes {
            tracing::info!("creating account-secret hash table (SQL file 1/2)");
            sqlx::raw_sql(CREATE)
                .execute(&mut *connection)
                .await
                .context("auth_secret_hash_01_create.sql failed")?;
        }
        tracing::info!("backfilling and cutting over account secrets (SQL file 2/2)");
        sqlx::raw_sql(BACKFILL)
            .execute(&mut *connection)
            .await
            .context("auth_secret_hash_02_backfill.sql failed; restart retries the backfill, keeping the committed table")?;
        tracing::info!("account-secret hash cutover committed");
    }

    // Also fail before serving if the application search path cannot resolve the
    // destination, or an existing table has an incompatible column layout.
    sqlx::query("SELECT id, secret_hash FROM auth_secret_hash LIMIT 0")
        .execute(&mut *connection)
        .await
        .context("Account-secret hash table is not usable")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;

    const FIXTURE: &str = include_str!("../../tests/secret_hash_fixture.sql");

    #[tokio::test]
    #[ignore = "requires GEPH_TEST_DATABASE_URL pointing to disposable loopback PostgreSQL"]
    async fn startup_rollout_resumes_and_serializes() -> anyhow::Result<()> {
        let options = PgConnectOptions::from_str(&std::env::var("GEPH_TEST_DATABASE_URL")?)?;
        ensure!(
            matches!(options.get_host(), "127.0.0.1" | "localhost" | "::1"),
            "rollout tests require a loopback PostgreSQL server"
        );
        let mut admin = PgConnection::connect_with(&options).await?;
        let name = format!("geph_rollout_test_{:032x}", rand::random::<u128>());
        sqlx::raw_sql(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        let result = exercise_rollout(options.database(&name)).await;
        sqlx::raw_sql(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .execute(&mut admin)
            .await?;
        result
    }

    async fn exercise_rollout(options: PgConnectOptions) -> anyhow::Result<()> {
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options)
            .await?;
        let result = async {
            // Wrong/empty database fails before listeners can start.
            assert!(
                run(&pool)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Neither")
            );
            sqlx::raw_sql(FIXTURE).execute(&pool).await?;

            // A simultaneous starter does not wait on the migration lock or
            // start using the database while another broker is backfilling it.
            let mut holder = pool.acquire().await?.detach();
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(ROLLOUT_LOCK)
                .execute(&mut holder)
                .await?;
            let failure = tokio::time::timeout(std::time::Duration::from_secs(2), run(&pool))
                .await?
                .unwrap_err();
            assert!(failure.to_string().contains("Another broker"));
            assert!(!table_exists(&pool, "auth_secret_hash").await?);
            holder.close().await?;

            // Both scripts run through SQLx, with their own BEGIN/COMMIT.
            run(&pool).await?;
            verify_cutover(&pool).await?;
            run(&pool).await?;
            verify_cutover(&pool).await?;

            // Recreate only this generated test database's fixture. Force the
            // second script to fail at DROP, after it has done the backfill.
            sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
                .execute(&pool)
                .await?;
            sqlx::raw_sql(FIXTURE).execute(&pool).await?;
            sqlx::raw_sql("CREATE VIEW secret_dependency AS SELECT id FROM auth_secret")
                .execute(&pool)
                .await?;
            let failure = run(&pool).await.unwrap_err();
            assert!(failure.to_string().contains("02_backfill.sql failed"));
            assert!(table_exists(&pool, "auth_secret").await?);
            assert!(table_exists(&pool, "auth_secret_hash").await?);
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_secret_hash")
                .fetch_one(&pool)
                .await?;
            assert_eq!(count, 0);
            sqlx::raw_sql("DROP VIEW secret_dependency")
                .execute(&pool)
                .await?;
            // Also proves the failed attempt released its session lock and
            // aborted transaction, and the first file committed independently.
            run(&pool).await?;
            verify_cutover(&pool).await?;
            anyhow::Ok(())
        }
        .await;
        pool.close().await;
        result
    }

    async fn table_exists(pool: &PgPool, name: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{name}"))
            .fetch_one(pool)
            .await?)
    }

    async fn verify_cutover(pool: &PgPool) -> anyhow::Result<()> {
        assert!(!table_exists(pool, "auth_secret").await?);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_secret_hash")
            .fetch_one(pool)
            .await?;
        assert_eq!(count, 3);
        let matched: bool = sqlx::query_scalar(
            "SELECT secret_hash = sha256(convert_to('000123', 'UTF8')) FROM auth_secret_hash WHERE id = 2",
        ).fetch_one(pool).await?;
        assert!(matched);
        let code: String = sqlx::query_scalar("SELECT code FROM invite_codes WHERE user_id = 2")
            .fetch_one(pool)
            .await?;
        assert_eq!(code, "EXISTING-CODE");
        let version: i64 = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_one(pool)
            .await?;
        assert_eq!(version, 123);
        Ok(())
    }
}
