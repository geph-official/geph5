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
    // returns (current usage, optional limit)
    let (mb_used, mb_limit): (i32, Option<i32>) = sqlx::query_as(
        r#"
WITH updated AS (
    INSERT INTO bw_usage (id, mb_used)
    VALUES ($1, $2)
    ON CONFLICT (id) DO UPDATE
      SET mb_used = bw_usage.mb_used + EXCLUDED.mb_used
    RETURNING id, mb_used
)
SELECT u.mb_used, l.mb_limit
FROM updated AS u
LEFT JOIN bw_limits AS l USING (id);
"#,
    )
    .bind(user_id)
    .bind(mbs)
    .fetch_one(&*POSTGRES)
    .await?;

    if let Some(limit) = mb_limit {
        if mb_used > limit {
            anyhow::bail!("consumed over limit");
        }
    }

    Ok(())
}
