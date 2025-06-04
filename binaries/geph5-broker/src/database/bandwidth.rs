use super::POSTGRES;

pub async fn consume_bw(user_id: i32, mbs: i32) -> anyhow::Result<()> {
    sqlx::query("insert into bw_usage (id, mb_used) values ($1, $2) on conflict(id) do update set mb_used = bw_usage.mb_used + EXCLUDED.mb_used")
        .bind(user_id)
        .bind(mbs)
        .execute(&*POSTGRES)
        .await?;
    Ok(())
}
