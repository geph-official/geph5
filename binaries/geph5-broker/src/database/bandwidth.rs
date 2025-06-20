use geph5_broker_protocol::BwConsumptionInfo;

use super::POSTGRES;

pub async fn basic_count() -> anyhow::Result<i32> {
    let count: i32 = sqlx::query_scalar(
        "select count(distinct user_id) from plus_periods where end_time > NOW() and tier = 0",
    )
    .fetch_one(&*POSTGRES)
    .await?;
    Ok(count)
}

pub async fn bw_consumption(user_id: i32) -> anyhow::Result<Option<BwConsumptionInfo>> {
    let pair: Option<(i32, i32, i64)> = sqlx::query_as("select (renew_mb - (mb_limit - mb_used)), renew_mb, extract(epoch from renew_date)::bigint from bw_usage natural join bw_limits where id = $1").bind(user_id).fetch_optional(&*POSTGRES).await?;
    Ok(pair.map(|pair| BwConsumptionInfo {
        mb_used: pair.0.max(0) as _,
        mb_limit: pair.1.max(0) as _,
        renew_unix: pair.2 as _,
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

    if let Some(limit) = limit {
        if mb_used > limit {
            anyhow::bail!("consumed over limit")
        }
    }
    txn.commit().await?;
    Ok(())
}
