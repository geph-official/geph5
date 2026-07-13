use geph5_broker_protocol::BwConsumptionInfo;
use mizaru2::ClientToken;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use super::POSTGRES;

/// Ensure the `spent_bw_tokens` table exists. Runs at most once per process,
/// rather than on every redemption (the previous per-call DDL added a catalog
/// round-trip to the hot path).
async fn ensure_spent_bw_tokens_schema() -> anyhow::Result<()> {
    static DONE: OnceCell<()> = OnceCell::const_new();
    DONE.get_or_try_init(|| async {
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS spent_bw_tokens (
                token_hash bytea primary key,
                consumed_at timestamptz not null default now()
            )"#,
        )
        .execute(&*POSTGRES)
        .await?;
        anyhow::Ok(())
    })
    .await
    .copied()
}

/// Record a bandwidth token as spent. Returns `Ok(true)` if this is the first
/// time the token is seen (i.e. it is fresh and should be credited), and
/// `Ok(false)` if the token has already been consumed (a replay that must be
/// rejected).
///
/// A transient database error is retried a few times before giving up, so a
/// momentary hiccup (deadlock, pool timeout) does not get reported to the exit
/// as a permanent rejection — which would burn a legitimate, unspent token.
pub async fn record_spent_bw_token(token: ClientToken) -> anyhow::Result<bool> {
    ensure_spent_bw_tokens_schema().await?;
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let token_hash: Vec<u8> = hasher.finalize().to_vec();

    let mut last_err = None;
    for attempt in 0..3 {
        match sqlx::query_as::<_, (Vec<u8>,)>(
            "INSERT INTO spent_bw_tokens (token_hash) VALUES ($1)
             ON CONFLICT (token_hash) DO NOTHING
             RETURNING token_hash",
        )
        .bind(&token_hash)
        .fetch_optional(&*POSTGRES)
        .await
        {
            Ok(inserted) => return Ok(inserted.is_some()),
            Err(err) => {
                tracing::warn!(attempt, error = %err, "transient error recording spent bw token, retrying");
                last_err = Some(err);
                tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
            }
        }
    }
    Err(last_err.unwrap().into())
}

pub async fn bw_consumption(user_id: i32) -> anyhow::Result<Option<BwConsumptionInfo>> {
    let total_mb_used: i32 = sqlx::query_scalar("select mb_used from bw_usage where id = $1")
        .bind(user_id)
        .fetch_optional(&*POSTGRES)
        .await?
        .unwrap_or(0);
    let renewmb_mblimit: Option<(i32, i32, i64)> = sqlx::query_as("select renew_mb, mb_limit, extract(epoch from renew_date)::bigint from bw_limits where id = $1")
        .bind(user_id)
        .fetch_optional(&*POSTGRES)
        .await?;

    Ok(renewmb_mblimit.map(|pair| {
        let renew_mb = pair.0;
        let mb_limit = pair.1;
        BwConsumptionInfo {
            mb_used: (renew_mb - (mb_limit - total_mb_used)) as _,
            mb_limit: renew_mb as _,
            renew_unix: pair.2 as _,
        }
    }))
}

pub async fn consume_bw(user_id: i32, mbs: i32) -> anyhow::Result<()> {
    let mut txn = POSTGRES.begin().await?;

    let mb_used: i32 = sqlx::query_scalar(
        "INSERT INTO bw_usage (id, mb_used)
VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE
  SET mb_used = bw_usage.mb_used + EXCLUDED.mb_used
RETURNING mb_used;
",
    )
    .bind(user_id)
    .bind(mbs)
    .fetch_one(&mut *txn)
    .await?;

    // then check against the bandwidth limits
    let limit: Option<i32> = sqlx::query_scalar("select mb_limit from bw_limits where id = $1")
        .bind(user_id)
        .fetch_optional(&mut *txn)
        .await?;

    if let Some(limit) = limit
        && mb_used > limit
    {
        anyhow::bail!("consumed over limit")
    }
    txn.commit().await?;
    Ok(())
}
