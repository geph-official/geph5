use anyhow::{Context, ensure};
use sqlx::{Connection, PgConnection, PgPool};

const ACCOUNT_CREATE: &str = include_str!("../../sql/auth_secret_hash_01_create.sql");
const ACCOUNT_BACKFILL: &str = include_str!("../../sql/auth_secret_hash_02_backfill.sql");
const TOKEN_CREATE: &str = include_str!("../../sql/auth_token_hash_01_create.sql");
const TOKEN_BACKFILL: &str = include_str!("../../sql/auth_token_hash_02_backfill.sql");
const SECRET_RECOVERY: &str = include_str!("../../sql/auth_secret_recovery_01_create.sql");
const SECRET_HISTORY: &str = include_str!("../../sql/auth_secret_history_01_create.sql");
const ROTATION_REWARD_CREATE: &str =
    include_str!("../../sql/auth_secret_rotation_rewards_01_create.sql");
pub(super) const ROTATION_REWARD_GRANT: &str =
    include_str!("../../sql/auth_secret_rotation_rewards_02_grant.sql");
// Session lock spans all separately committed SQL files. It only coordinates
// new brokers; old account writers must be stopped for the cutover.
const ROLLOUT_LOCK: i64 = 0x6765706853686132;

pub async fn run(pool: &PgPool) -> anyhow::Result<()> {
    // Never return a session carrying an advisory lock or an aborted script
    // transaction to the application pool. Drop also closes it on cancellation.
    let mut connection = pool.acquire().await?.detach();
    let result = run_on_connection(&mut connection).await;
    let closed = connection.close().await;
    result?;
    closed.context("Closing secret hash rollout connection")?;
    Ok(())
}

async fn run_on_connection(connection: &mut PgConnection) -> anyhow::Result<()> {
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(ROLLOUT_LOCK)
        .fetch_one(&mut *connection)
        .await?;
    ensure!(
        locked,
        "Another broker is running the secret hash rollout; retry startup after it finishes"
    );

    run_pair(
        connection,
        "auth_secret",
        "auth_secret_hash",
        ("auth_secret_hash_01_create.sql", ACCOUNT_CREATE),
        ("auth_secret_hash_02_backfill.sql", ACCOUNT_BACKFILL),
        "SELECT id, secret_hash FROM auth_secret_hash LIMIT 0",
    )
    .await?;
    run_pair(
        connection,
        "auth_tokens",
        "auth_token_hash",
        ("auth_token_hash_01_create.sql", TOKEN_CREATE),
        ("auth_token_hash_02_backfill.sql", TOKEN_BACKFILL),
        "SELECT user_id, token_hash FROM auth_token_hash LIMIT 0",
    )
    .await?;
    sqlx::raw_sql(SECRET_HISTORY)
        .execute(&mut *connection)
        .await
        .context("Creating account secret history")?;
    sqlx::query("SELECT secret_hash, user_id, retired_at FROM auth_secret_history LIMIT 0")
        .execute(&mut *connection)
        .await
        .context("Account secret history is not usable")?;
    sqlx::raw_sql(SECRET_RECOVERY)
        .execute(&mut *connection)
        .await
        .context("Creating account secret recovery storage")?;
    sqlx::query("SELECT secret_hash, replacement, expires_at FROM auth_secret_recovery LIMIT 0")
        .execute(&mut *connection)
        .await
        .context("Account secret recovery storage is not usable")?;
    sqlx::raw_sql(ROTATION_REWARD_CREATE)
        .execute(&mut *connection)
        .await
        .context("Creating account rotation reward tracking")?;
    let mut txn = connection.begin().await?;
    sqlx::raw_sql("SET LOCAL lock_timeout = '250ms'; SET LOCAL statement_timeout = '5min';")
        .execute(&mut *txn)
        .await?;
    let rewarded = sqlx::query(ROTATION_REWARD_GRANT)
        .bind(None::<i32>)
        .execute(&mut *txn)
        .await
        .context("Backfilling account rotation subscription rewards")?
        .rows_affected();
    txn.commit().await?;
    tracing::info!(
        rewarded,
        "account rotation subscription reward backfill complete"
    );
    Ok(())
}

async fn run_pair(
    connection: &mut PgConnection,
    source: &str,
    destination: &str,
    create: (&str, &str),
    backfill: (&str, &str),
    validation: &str,
) -> anyhow::Result<()> {
    let (plaintext, hashes): (bool, bool) =
        sqlx::query_as("SELECT to_regclass($1) IS NOT NULL, to_regclass($2) IS NOT NULL")
            .bind(format!("public.{source}"))
            .bind(format!("public.{destination}"))
            .fetch_one(&mut *connection)
            .await?;
    ensure!(
        plaintext || hashes,
        "Neither {source} nor {destination} exists; check the broker database configuration"
    );

    if plaintext {
        if !hashes {
            tracing::info!(file = create.0, "creating hash table");
            sqlx::raw_sql(create.1)
                .execute(&mut *connection)
                .await
                .with_context(|| format!("{} failed", create.0))?;
        }
        tracing::info!(file = backfill.0, "backfilling and cutting over to hashes");
        sqlx::raw_sql(backfill.1)
            .execute(&mut *connection)
            .await
            .with_context(|| {
                format!(
                    "{} failed; restart retries the backfill, keeping the committed table",
                    backfill.0
                )
            })?;
        tracing::info!(destination, "secret hash cutover committed");
    }

    // Also fail before serving if the application search path cannot resolve the
    // destination, or an existing table has an incompatible column layout.
    sqlx::query(validation)
        .execute(&mut *connection)
        .await
        .with_context(|| format!("{destination} table is not usable"))?;
    Ok(())
}
