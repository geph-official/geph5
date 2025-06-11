use super::POSTGRES;

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
    SET    mb_limit   = mb_limit + renew_mb,
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
