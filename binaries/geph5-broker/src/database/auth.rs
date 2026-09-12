use argon2::{Argon2, PasswordHash, PasswordVerifier, password_hash::Encoding};

use cached::proc_macro::cached;
use geph5_broker_protocol::{AccountLevel, AuthError, Credential, UserInfo};

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use moka::future::Cache;
use rand::Rng as _;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, types::chrono::Utc};

use super::POSTGRES;
use crate::{database::bandwidth::bw_consumption, log_error};

pub async fn register_secret() -> anyhow::Result<String> {
    register_secret_in_pool(&POSTGRES).await
}

async fn register_secret_in_pool(pool: &PgPool) -> anyhow::Result<String> {
    let mut txn = pool.begin().await?;
    let (user_id,): (i32,) =
        sqlx::query_as("INSERT INTO users (createtime) VALUES (NOW()) RETURNING id")
            .fetch_one(&mut *txn)
            .await?;

    let secret = (0..23)
        .map(|_| rand::thread_rng().gen_range(0..9))
        .fold(String::from("9"), |a, b| format!("{a}{b}"));

    sqlx::query("INSERT INTO auth_secret_hash (id, secret_hash) VALUES ($1, $2)")
        .bind(user_id)
        .bind(secret_hash(&secret).as_slice())
        .execute(&mut *txn)
        .await?;
    // The GUI derives this locally from the original secret. Persist the code
    // while we still have that secret, in the same transaction as the account.
    sqlx::query("INSERT INTO invite_codes (user_id, code) VALUES ($1, $2)")
        .bind(user_id)
        .bind(secret_to_invite_code(&secret))
        .execute(&mut *txn)
        .await?;

    txn.commit().await?;
    Ok(secret)
}

fn secret_hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn secret_to_invite_code(secret: &str) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let digest = Sha256::new()
        .chain_update(b"invite-code")
        .chain_update(secret.as_bytes())
        .finalize();
    let mut code = String::with_capacity(16);
    for chunk in digest[..10].chunks_exact(5) {
        let block = chunk.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
        for group in (0..8).rev() {
            code.push(ALPHABET[((block >> (group * 5)) & 31) as usize] as char);
        }
    }
    code
}

pub async fn validate_credential(credential: Credential) -> Result<i32, AuthError> {
    match credential {
        Credential::TestDummy => Err(AuthError::Forbidden),
        Credential::LegacyUsernamePassword { username, password } => {
            Ok(validate_username_pwd(&username, &password).await?)
        }
        Credential::Secret(s) => Ok(validate_secret(&s).await?),
    }
}

pub async fn validate_secret(secret: &str) -> Result<i32, AuthError> {
    validate_secret_in_pool(&POSTGRES, secret).await
}

async fn validate_secret_in_pool(pool: &PgPool, secret: &str) -> Result<i32, AuthError> {
    let res: Option<(i32,)> =
        sqlx::query_as("SELECT id FROM auth_secret_hash WHERE secret_hash = $1")
            .bind(secret_hash(secret).as_slice())
            .fetch_optional(pool)
            .await
            .inspect_err(log_error)
            .map_err(|_| AuthError::RateLimited)?;

    // If we find a matching user_id, great; otherwise, Forbidden.
    if let Some((user_id,)) = res {
        Ok(user_id)
    } else {
        Err(AuthError::Forbidden)
    }
}

pub async fn validate_username_pwd(username: &str, password: &str) -> Result<i32, AuthError> {
    tracing::debug!(username, "validating legacy username/password");
    let res: Option<(i32, String)> =
        sqlx::query_as("select user_id,pwdhash from auth_password where username = $1")
            .bind(username)
            .fetch_optional(&*POSTGRES)
            .await
            .inspect_err(log_error)
            .map_err(|_| AuthError::RateLimited)?;
    let (user_id, phc_string) = if let Some(res) = res {
        res
    } else {
        return Err(AuthError::Forbidden);
    };

    let phc = PasswordHash::parse(&phc_string, Encoding::B64)
        .inspect_err(log_error)
        .map_err(|_| AuthError::Forbidden)?;

    Argon2::default()
        .verify_password(password.as_bytes(), &phc)
        .map_err(|_| AuthError::Forbidden)?;

    Ok(user_id)
}

pub async fn new_auth_token(user_id: i32) -> anyhow::Result<String> {
    new_auth_token_in_pool(&POSTGRES, user_id).await
}

async fn new_auth_token_in_pool(pool: &PgPool, user_id: i32) -> anyhow::Result<String> {
    let token: String = std::iter::repeat(())
        .map(|()| rand::thread_rng().sample(rand::distributions::Alphanumeric))
        .map(char::from)
        .take(30)
        .collect();

    match sqlx::query("INSERT INTO auth_token_hash (token_hash, user_id) VALUES ($1, $2)")
        .bind(secret_hash(&token).as_slice())
        .bind(user_id)
        .execute(pool)
        .await
    {
        Ok(_) => Ok(token),
        Err(e) => anyhow::bail!("database failed {e}"), // If insertion fails, return RateLimited error
    }
}

#[cached(time = 86400, result = true)]
async fn get_user_id_from_token(token_hash: [u8; 32]) -> anyhow::Result<Option<i32>> {
    get_user_id_from_token_hash_in_pool(&POSTGRES, &token_hash).await
}

async fn get_user_id_from_token_hash_in_pool(
    pool: &PgPool,
    token_hash: &[u8; 32],
) -> anyhow::Result<Option<i32>> {
    let user_id: Option<(i32,)> =
        sqlx::query_as("SELECT user_id FROM auth_token_hash WHERE token_hash = $1")
            .bind(token_hash.as_slice())
            .fetch_optional(pool)
            .await?;

    Ok(user_id.map(|(user_id,)| user_id))
}

// Refactored function that uses the helper without caching its own result
pub async fn valid_auth_token(token: String) -> anyhow::Result<Option<(i32, AccountLevel)>> {
    let user_id = match get_user_id_from_token(secret_hash(&token)).await? {
        Some(id) => id,
        None => return Ok(None),
    };

    let expiry = get_subscription_expiry(user_id).await?;
    tracing::trace!(user_id, expiry = debug(expiry), "valid auth token");
    geph5_rt::spawn(record_auth(user_id)).detach();

    if expiry.is_none() {
        Ok(Some((user_id, AccountLevel::Free)))
    } else {
        Ok(Some((user_id, AccountLevel::Plus)))
    }
}

pub async fn get_user_info(user_id: i32) -> Result<Option<UserInfo>, AuthError> {
    let plus_expires_unix = get_subscription_expiry(user_id)
        .await
        .map_err(|_| AuthError::RateLimited)?;
    tracing::debug!(
        user_id,
        plus_expires_unix = debug(plus_expires_unix),
        "got expires unix"
    );

    Ok(Some(UserInfo {
        user_id: user_id as _,
        plus_expires_unix: plus_expires_unix.map(|s| s.0 as _),
        recurring: plus_expires_unix.map(|s| s.1).unwrap_or_default(),
        bw_consumption: bw_consumption(user_id).await.map_err(|e| {
            tracing::warn!(err = debug(e), "cannot get bw consumption");
            AuthError::RateLimited
        })?,
    }))
}

pub async fn get_subscription_expiry(user_id: i32) -> anyhow::Result<Option<(i64, bool)>> {
    static ALL_SUBSCRIPTIONS_CACHE: LazyLock<Cache<i32, Arc<BTreeMap<i32, (i64, bool)>>>> =
        LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .build()
        });
    static PERIOD_COUNT_CACHE: LazyLock<Cache<u128, i32>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_idle(Duration::from_secs(60))
            .build()
    });
    let start = Instant::now();

    let mut ts_missed = false;
    let period_count = PERIOD_COUNT_CACHE
        .try_get_with(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                / 500,
            async {
                let ts = sqlx::query_scalar::<_, i32>("SELECT max(period_id) FROM plus_periods")
                    .fetch_one(&*POSTGRES)
                    .await?;
                ts_missed = true;
                anyhow::Ok(ts)
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut sub_missed = false;
    let all_subscriptions = ALL_SUBSCRIPTIONS_CACHE
        .try_get_with(period_count, async {
            let all_subscriptions: Vec<(i32, i64, bool)> = sqlx::query_as(
                "SELECT 
    s.id, 
    EXTRACT(EPOCH FROM s.expires)::bigint AS unix_timestamp,
    (r.user_id IS NOT NULL) AS has_recurring
FROM subscriptions s
LEFT JOIN stripe_recurring r ON s.id = r.user_id",
            )
            .fetch_all(&*POSTGRES)
            .await?;
            sub_missed = true;
            anyhow::Ok(Arc::new(
                all_subscriptions
                    .into_iter()
                    .map(|(id, expires, recur)| (id, (expires, recur)))
                    .collect(),
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if rand::random::<f64>() < 0.1 {
        tracing::debug!(
            "sub expiry missed? {ts_missed} {sub_missed} elapsed={:?}",
            start.elapsed()
        )
    }
    Ok(all_subscriptions.get(&user_id).cloned())
}

pub async fn record_auth(user_id: i32) -> anyhow::Result<()> {
    let now = Utc::now().naive_utc();

    sqlx::query(
        r#"INSERT INTO last_login (id, login_time)
VALUES ($1, $2)
ON CONFLICT (id) 
DO UPDATE SET login_time = EXCLUDED.login_time;
"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(&*POSTGRES)
    .await?;

    Ok(())
}

pub async fn delete_user_by_secret(secret: &str) -> anyhow::Result<()> {
    sqlx::query(
        "delete from users where id=(select id from auth_secret_hash where secret_hash=$1)",
    )
    .bind(secret_hash(secret).as_slice())
    .execute(&*POSTGRES)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{
        Executor,
        postgres::{PgConnectOptions, PgPoolOptions},
    };
    use std::str::FromStr;

    #[test]
    fn secret_hash_and_referral_vectors() {
        assert_eq!(
            hex::encode(secret_hash("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Also checked against PostgreSQL's backfill and the GUI's Crockford
        // encoding: leading zeroes and exact bytes must not be normalized.
        assert_ne!(secret_hash("01"), secret_hash("1"));
        assert_eq!(
            secret_to_invite_code("900000000000000000000001"),
            "XRZ1GB4DF6YMV2SN"
        );
    }

    async fn temporary_account_pool() -> anyhow::Result<PgPool> {
        let options = PgConnectOptions::from_str(&std::env::var("GEPH_TEST_DATABASE_URL")?)?;
        anyhow::ensure!(
            matches!(options.get_host(), "127.0.0.1" | "localhost" | "::1"),
            "account tests require a loopback PostgreSQL server"
        );
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_with(options)
            .await?;
        pool.execute("CREATE TEMP TABLE users (id SERIAL PRIMARY KEY, createtime TIMESTAMP NOT NULL);
            CREATE TEMP TABLE auth_secret_hash (id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
                secret_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(secret_hash) = 32));
            CREATE TEMP TABLE invite_codes (user_id INTEGER PRIMARY KEY REFERENCES users(id), code TEXT NOT NULL UNIQUE);")
            .await?;
        Ok(pool)
    }

    #[tokio::test]
    #[ignore = "requires GEPH_TEST_DATABASE_URL pointing to disposable loopback PostgreSQL"]
    async fn registration_and_authentication_use_hashes() -> anyhow::Result<()> {
        let pool = temporary_account_pool().await?;
        let secret = register_secret_in_pool(&pool).await?;
        assert_eq!(secret.len(), 24);
        assert!(secret.starts_with('9'));
        assert!(secret[1..].bytes().all(|b| (b'0'..=b'8').contains(&b)));
        let id = validate_secret_in_pool(&pool, &secret).await?;
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT secret_hash FROM auth_secret_hash WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        let expected: Vec<u8> = sqlx::query_scalar("SELECT sha256(convert_to($1, 'UTF8'))")
            .bind(&secret)
            .fetch_one(&pool)
            .await?;
        assert_eq!(stored, expected);
        let code: String = sqlx::query_scalar("SELECT code FROM invite_codes WHERE user_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(code, secret_to_invite_code(&secret));
        for invalid in [
            "wrong".to_string(),
            hex::encode(stored),
            format!(" {secret}"),
        ] {
            assert!(matches!(
                validate_secret_in_pool(&pool, &invalid).await,
                Err(AuthError::Forbidden)
            ));
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GEPH_TEST_DATABASE_URL pointing to disposable loopback PostgreSQL"]
    async fn referral_failure_rolls_back_registration() -> anyhow::Result<()> {
        let pool = temporary_account_pool().await?;
        pool.execute("ALTER TABLE invite_codes ADD CONSTRAINT simulate_failure CHECK (false)")
            .await?;
        assert!(register_secret_in_pool(&pool).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_secret_hash")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GEPH_TEST_DATABASE_URL pointing to disposable loopback PostgreSQL"]
    async fn device_tokens_store_and_validate_only_hashes() -> anyhow::Result<()> {
        let pool = temporary_account_pool().await?;
        pool.execute(
            "CREATE TEMP TABLE auth_token_hash (
            token_hash BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
            user_id INTEGER NOT NULL)",
        )
        .await?;
        let first = new_auth_token_in_pool(&pool, 42).await?;
        let second = new_auth_token_in_pool(&pool, 42).await?;
        assert_eq!(first.len(), 30);
        assert!(first.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(first, second);
        for token in [&first, &second] {
            let hash = secret_hash(token);
            assert_eq!(
                get_user_id_from_token_hash_in_pool(&pool, &hash).await?,
                Some(42)
            );
            let stored: Vec<u8> = sqlx::query_scalar(
                "SELECT token_hash FROM auth_token_hash WHERE token_hash = sha256(convert_to($1, 'UTF8'))",
            ).bind(token).fetch_one(&pool).await?;
            assert_eq!(stored, hash);
            for invalid in [
                hex::encode(stored),
                format!(" {token}"),
                "wrong".to_string(),
            ] {
                assert_eq!(
                    get_user_id_from_token_hash_in_pool(&pool, &secret_hash(&invalid)).await?,
                    None
                );
            }
        }
        sqlx::query("DELETE FROM auth_token_hash WHERE token_hash = $1")
            .bind(secret_hash(&first).as_slice())
            .execute(&pool)
            .await?;
        assert_eq!(
            get_user_id_from_token_hash_in_pool(&pool, &secret_hash(&first)).await?,
            None
        );
        assert_eq!(
            get_user_id_from_token_hash_in_pool(&pool, &secret_hash(&second)).await?,
            Some(42)
        );
        pool.close().await;
        Ok(())
    }
}
