//! Account codes, credential verification, broker tokens, and account metadata.

use argon2::{Argon2, PasswordHash, PasswordVerifier, password_hash::Encoding};
use cached::proc_macro::cached;
use geph5_broker_protocol::{
    AccountLevel, AccountSecretError, AccountSecretStatus, AuthError, Credential, UserInfo,
};
use moka::future::Cache;
use rand::Rng as _;
use sha2::{Digest, Sha256};
use sqlx::{Executor, PgConnection, Postgres, types::chrono::Utc};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, LazyLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{POSTGRES, bandwidth::bw_consumption};
use crate::log_error;

pub async fn register_secret() -> anyhow::Result<String> {
    retry_serializable(|| async {
        let mut txn = POSTGRES.begin().await?;
        let secret = (0..23)
            .map(|_| rand::thread_rng().gen_range(0..10))
            .fold(String::from("8"), |a, b| format!("{a}{b}"));
        let (user_id,): (i32,) =
            sqlx::query_as("INSERT INTO users (createtime) VALUES (NOW()) RETURNING id")
                .fetch_one(&mut *txn)
                .await?;

        sqlx::query("INSERT INTO auth_secret_hash (id, secret_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(credential_hash(&secret).as_slice())
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
    })
    .await
}

pub async fn get_account_secret_status(
    secret: &str,
) -> Result<AccountSecretStatus, AccountSecretError> {
    // One snapshot prevents a concurrent move into history looking like Invalid.
    let row: Option<(i32, bool, Option<String>)> = sqlx::query_as(
        "SELECT s.id, false, i.code FROM auth_secret_hash s
         LEFT JOIN invite_codes i ON i.user_id=s.id WHERE s.secret_hash=$1
         UNION ALL
         SELECT user_id, true, NULL::text FROM auth_secret_history WHERE secret_hash=$1",
    )
    .bind(credential_hash(secret).as_slice())
    .fetch_optional(&*POSTGRES)
    .await
    .map_err(account_secret_error)?;
    Ok(match row {
        Some((_, true, _)) => AccountSecretStatus::Retired,
        Some((user_id, false, invite_code)) => AccountSecretStatus::Current {
            user_id: user_id as u64,
            invite_code,
        },
        None => AccountSecretStatus::Invalid,
    })
}

pub async fn rotate_account_secret(current_secret: &str) -> Result<String, AccountSecretError> {
    static REPLACEMENTS: LazyLock<Cache<[u8; 32], String>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(600))
            .build()
    });
    if !current_secret.starts_with('9') {
        return Err(AccountSecretError::Forbidden);
    }
    let current = credential_hash(current_secret);
    // Coalesce concurrent calls and retain successful replacements for retries.
    REPLACEMENTS
        .try_get_with(current, async {
            retry_serializable(|| async {
                let mut txn = POSTGRES.begin().await?;
                let user_id: Option<i32> =
                    sqlx::query_scalar("SELECT id FROM auth_secret_hash WHERE secret_hash=$1")
                        .bind(current.as_slice())
                        .fetch_optional(&mut *txn)
                        .await?;
                let user_id = match user_id {
                    Some(user_id) => user_id,
                    None => {
                        let retired: bool = sqlx::query_scalar(
                            "SELECT EXISTS(SELECT 1 FROM auth_secret_history WHERE secret_hash=$1)",
                        )
                        .bind(current.as_slice())
                        .fetch_one(&mut *txn)
                        .await?;
                        return Err(if retired {
                            AccountSecretError::Retired
                        } else {
                            AccountSecretError::Forbidden
                        }
                        .into());
                    }
                };
                let replacement_secret = (0..23)
                    .map(|_| rand::thread_rng().gen_range(0..10))
                    .fold(String::from("8"), |a, b| format!("{a}{b}"));
                sqlx::query(
                    "INSERT INTO auth_secret_history (secret_hash, user_id) VALUES ($1, $2)",
                )
                .bind(current.as_slice())
                .bind(user_id)
                .execute(&mut *txn)
                .await?;
                sqlx::query("UPDATE auth_secret_hash SET secret_hash=$1 WHERE id=$2")
                    .bind(credential_hash(&replacement_secret).as_slice())
                    .bind(user_id)
                    .execute(&mut *txn)
                    .await?;
                sqlx::query("DELETE FROM auth_token_hash WHERE user_id=$1")
                    .bind(user_id)
                    .execute(&mut *txn)
                    .await?;
                txn.commit().await?;
                // Existing process-local token caches intentionally survive until expiry.
                Ok(replacement_secret)
            })
            .await
            .map_err(account_secret_error)
        })
        .await
        .map_err(|error| (*error).clone())
}

pub async fn validate_credential(credential: Credential) -> Result<i32, AuthError> {
    let mut connection = POSTGRES.acquire().await.map_err(auth_error)?;
    validate_credential_in_connection(&mut connection, &credential)
        .await
        .map_err(auth_error)
}

pub async fn validate_secret(secret: &str) -> Result<i32, AuthError> {
    user_id_by_secret(&*POSTGRES, secret)
        .await
        .map_err(auth_error)?
        .ok_or(AuthError::Forbidden)
}

pub async fn issue_auth_token(credential: Credential) -> Result<String, AuthError> {
    retry_serializable(|| async {
        let mut txn = POSTGRES.begin().await?;
        let user_id = validate_credential_in_connection(&mut txn, &credential).await?;
        let token: String = std::iter::repeat(())
            .map(|()| rand::thread_rng().sample(rand::distributions::Alphanumeric))
            .map(char::from)
            .take(30)
            .collect();

        sqlx::query("INSERT INTO auth_token_hash (token_hash, user_id) VALUES ($1, $2)")
            .bind(credential_hash(&token).as_slice())
            .bind(user_id)
            .execute(&mut *txn)
            .await?;
        txn.commit().await?;
        Ok(token)
    })
    .await
    .map_err(auth_error)
}

pub async fn valid_auth_token(token: String) -> anyhow::Result<Option<(i32, AccountLevel)>> {
    let user_id = match get_user_id_from_token(credential_hash(&token)).await? {
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

pub async fn delete_user_by_secret(secret: &str) -> anyhow::Result<()> {
    sqlx::query(
        "delete from users where id=(select id from auth_secret_hash where secret_hash=$1)",
    )
    .bind(credential_hash(secret).as_slice())
    .execute(&*POSTGRES)
    .await?;
    Ok(())
}

fn credential_hash(credential: &str) -> [u8; 32] {
    Sha256::digest(credential.as_bytes()).into()
}

async fn validate_credential_in_connection(
    connection: &mut PgConnection,
    credential: &Credential,
) -> anyhow::Result<i32> {
    match credential {
        Credential::TestDummy => Err(AuthError::Forbidden.into()),
        Credential::LegacyUsernamePassword { username, password } => {
            validate_username_pwd(connection, username, password).await
        }
        Credential::Secret(s) => Ok(user_id_by_secret(connection, s)
            .await?
            .ok_or(AuthError::Forbidden)?),
    }
}

async fn validate_username_pwd(
    connection: &mut PgConnection,
    username: &str,
    password: &str,
) -> anyhow::Result<i32> {
    tracing::debug!(username, "validating legacy username/password");
    let res: Option<(i32, String)> =
        sqlx::query_as("SELECT user_id,pwdhash FROM auth_password p WHERE username=$1
                       AND NOT EXISTS (SELECT 1 FROM auth_secret_history h WHERE h.user_id=p.user_id)")
            .bind(username)
            .fetch_optional(connection)
            .await?;
    let (user_id, phc_string) = if let Some(res) = res {
        res
    } else {
        return Err(AuthError::Forbidden.into());
    };

    let phc = PasswordHash::parse(&phc_string, Encoding::B64)
        .inspect_err(log_error)
        .map_err(|_| AuthError::Forbidden)?;

    Argon2::default()
        .verify_password(password.as_bytes(), &phc)
        .map_err(|_| AuthError::Forbidden)?;

    Ok(user_id)
}

async fn user_id_by_secret<'e>(
    executor: impl Executor<'e, Database = Postgres>,
    secret: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM auth_secret_hash WHERE secret_hash = $1")
        .bind(credential_hash(secret).as_slice())
        .fetch_optional(executor)
        .await
}

#[cached(time = 86400, result = true)]
async fn get_user_id_from_token(token_hash: [u8; 32]) -> anyhow::Result<Option<i32>> {
    Ok(
        sqlx::query_scalar("SELECT user_id FROM auth_token_hash WHERE token_hash = $1")
            .bind(token_hash.as_slice())
            .fetch_optional(&*POSTGRES)
            .await?,
    )
}

async fn get_subscription_expiry(user_id: i32) -> anyhow::Result<Option<(i64, bool)>> {
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

async fn record_auth(user_id: i32) -> anyhow::Result<()> {
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

fn account_secret_error(error: impl Into<anyhow::Error>) -> AccountSecretError {
    let error = error.into();
    if let Some(error) = error.downcast_ref::<AccountSecretError>() {
        return error.clone();
    }
    tracing::warn!(error = %error, "account secret operation failed");
    match error.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::PoolTimedOut) => AccountSecretError::RateLimited,
        _ => AccountSecretError::Unavailable,
    }
}

fn auth_error(error: impl Into<anyhow::Error>) -> AuthError {
    let error = error.into();
    if let Some(error) = error.downcast_ref::<AuthError>() {
        return error.clone();
    }
    log_error(&error);
    AuthError::RateLimited
}

/// Retry the whole transaction, including reads, after an MVCC conflict.
/// Uses the database's default isolation; unique conflicts can require a retry too.
async fn retry_serializable<T, F, Fut>(mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    for attempt in 0..3 {
        let result = operation().await;
        let retry = result.as_ref().err().is_some_and(|error| {
            error
                .downcast_ref::<sqlx::Error>()
                .and_then(|error| error.as_database_error())
                .and_then(|error| error.code())
                .is_some_and(|code| matches!(code.as_ref(), "40001" | "40P01" | "23505"))
        });
        if !retry || attempt == 2 {
            return result;
        }
        tokio::task::yield_now().await;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_hash_and_referral_vectors() {
        assert_eq!(
            hex::encode(credential_hash("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Also checked against PostgreSQL's backfill and the GUI's Crockford
        // encoding: leading zeroes and exact bytes must not be normalized.
        assert_ne!(credential_hash("01"), credential_hash("1"));
        assert_eq!(
            secret_to_invite_code("900000000000000000000001"),
            "XRZ1GB4DF6YMV2SN"
        );
    }
}
