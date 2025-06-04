use super::POSTGRES;
use geph5_broker_protocol::ExitMetadata;
use sqlx::{prelude::FromRow, types::Json};

#[derive(FromRow)]
pub struct ExitRow {
    pub pubkey: [u8; 32],
    pub c2e_listen: String,
    pub b2e_listen: String,
    pub country: String,
    pub city: String,
    pub load: f32,
    pub expiry: i64,
}

#[derive(FromRow)]
pub struct ExitRowWithMetadata {
    pub pubkey: [u8; 32],
    pub c2e_listen: String,
    pub b2e_listen: String,
    pub country: String,
    pub city: String,
    pub load: f32,
    pub expiry: i64,
    pub metadata: Option<Json<ExitMetadata>>,
}

pub async fn insert_exit(exit: &ExitRow) -> anyhow::Result<()> {
    sqlx::query(
        r"INSERT INTO exits_new (pubkey, c2e_listen, b2e_listen, country, city,load, expiry)
        VALUES ($1, $2, $3, $4, $5, $6, extract(epoch from now()) + ($7 - extract(epoch from now())))
        ON CONFLICT (pubkey) DO UPDATE
        SET c2e_listen = EXCLUDED.c2e_listen,
            b2e_listen = EXCLUDED.b2e_listen,
            country = EXCLUDED.country,
            city = EXCLUDED.city,
            load = EXCLUDED.load,
            expiry = extract(epoch from now()) + (EXCLUDED.expiry - extract(epoch from now()))
        "
    )
    .bind(exit.pubkey)
    .bind(&exit.c2e_listen)
    .bind(&exit.b2e_listen)
    .bind(&exit.country)
    .bind(&exit.city)
    .bind(exit.load)
    .bind(exit.expiry)
    .execute(&*POSTGRES)
    .await?;
    Ok(())
}

pub async fn insert_exit_metadata(pubkey: [u8; 32], metadata: ExitMetadata) -> anyhow::Result<()> {
    sqlx::query("insert into exit_metadata (pubkey, metadata) values ($1, $2) on conflict (pubkey) do update set metadata = excluded.metadata")
        .bind(pubkey)
        .bind(Json(metadata))
        .execute(&*POSTGRES)
        .await?;
    Ok(())
}

pub async fn list_with_metadata() -> anyhow::Result<Vec<ExitRowWithMetadata>> {
    let rows = sqlx::query_as::<_, ExitRowWithMetadata>(
        "select * from exits_new natural left join exit_metadata",
    )
    .fetch_all(&*POSTGRES)
    .await?;
    Ok(rows)
}
