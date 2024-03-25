use std::ops::Deref as _;

use rand::Rng as _;

use crate::database::POSTGRES;

pub async fn new_auth_token(user_id: i64) -> anyhow::Result<String> {
    let token: String = std::iter::repeat(())
        .map(|()| rand::thread_rng().sample(rand::distributions::Alphanumeric))
        .map(char::from)
        .take(30)
        .collect();

    match sqlx::query("INSERT INTO auth_tokens (token, user_id) VALUES ($1, $2)")
        .bind(&token)
        .bind(user_id)
        .execute(POSTGRES.deref())
        .await
    {
        Ok(_) => Ok(token),
        Err(e) => anyhow::bail!("database failed {e}"), // If insertion fails, return RateLimited error
    }
}

pub async fn valid_auth_token(token: &str) -> anyhow::Result<bool> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM auth_tokens WHERE token = $1")
        .bind(token)
        .fetch_one(POSTGRES.deref())
        .await?;

    Ok(row.0 > 0)
}
