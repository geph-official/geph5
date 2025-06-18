use geph5_broker_protocol::BwConsumptionInfo;

use super::POSTGRES;

pub async fn bw_consumption(user_id: i32) -> anyhow::Result<Option<BwConsumptionInfo>> {
    let pair: Option<(i32, i32, i64)> = sqlx::query_as("select (renew_mb - (mb_limit - mb_used)), renew_mb, extract(epcoh from renew_date) from bw_usage natural join bw_limits where id = $1").bind(user_id).fetch_optional(&*POSTGRES).await?;
    Ok(pair.map(|pair| BwConsumptionInfo {
        mb_used: pair.0.max(0) as _,
        mb_left: pair.1.max(0) as _,
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
    let limit: Option<i32> = sqlx::query_scalar(
        "WITH upd AS (          -- run only when renewal is due
    UPDATE bw_limits
    SET    mb_limit   = mb_used + renew_mb,
           renew_date = renew_date + (renew_days * INTERVAL '1 day')
    WHERE  id = $1
      AND  renew_date < NOW()
    RETURNING mb_limit                       -- new limit after renewal
)
/* If upd changed a row we already have the answer;
   otherwise fall back to the unchanged row.          */
SELECT mb_limit
FROM   upd
UNION ALL
SELECT mb_limit
FROM   bw_limits
WHERE  id = $1
  AND  NOT EXISTS (SELECT 1 FROM upd)
LIMIT 1;
",
    )
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
