use std::collections::BTreeMap;

use anyhow::Context;
use geph5_broker_protocol::VoucherInfo;

use crate::{
    database::POSTGRES,
    payments::{PaymentClient, PaymentTransport},
    CONFIG_FILE,
};

pub async fn get_free_voucher(user_id: i32) -> anyhow::Result<Option<VoucherInfo>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "select voucher,description from free_vouchers natural join users where id = $1 limit 1",
    )
    .bind(user_id)
    .fetch_optional(&*POSTGRES)
    .await?;
    if let Some((voucher, description)) = row {
        let map: BTreeMap<String, String> = serde_json::from_str(&description)?;
        Ok(Some(VoucherInfo {
            code: voucher,
            explanation: map,
        }))
    } else {
        Ok(None)
    }
}

pub async fn delete_free_voucher(user_id: i32) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM free_vouchers WHERE id = $1")
        .bind(user_id)
        .execute(&*POSTGRES)
        .await?;

    // Returns true if any rows were affected (i.e., a voucher was deleted)
    Ok(result.rows_affected() > 0)
}

/// Creates a free 1-day voucher for every user ID in the auth_secret table.
/// This function will check if a user already has a voucher and skip those users.
pub async fn create_one_day_vouchers_for_all_users() -> anyhow::Result<u64> {
    // Get all user IDs from auth_secret
    let user_ids: Vec<(i32,)> = sqlx::query_as("SELECT id FROM auth_secret")
        .fetch_all(&*POSTGRES)
        .await?;

    // Count of created vouchers
    let mut created_count = 0;

    // Prepare voucher description for 1-day free Plus
    let voucher_description = include_str!("free_voucher_description.json");

    // Process each user
    for (user_id,) in user_ids {
        tracing::debug!("free voucher for {user_id}");
        // Create a transaction
        let mut txn = POSTGRES.begin().await?;

        // Check if user already has a voucher
        let existing_voucher: Option<(i32,)> =
            sqlx::query_as("SELECT id FROM free_vouchers WHERE id = $1 LIMIT 1")
                .bind(user_id)
                .fetch_optional(&mut *txn)
                .await?;

        // Skip if user already has a voucher
        if existing_voucher.is_some() {
            continue;
        }

        // Generate a new voucher code using the payment service
        let code = PaymentClient(PaymentTransport)
            .create_giftcard(CONFIG_FILE.wait().payment_support_secret.clone(), 1)
            .await?
            .map_err(|e| anyhow::anyhow!("Failed to create giftcard: {}", e))?;

        // Insert the voucher into the database
        sqlx::query(
            r#"
            INSERT INTO free_vouchers (id, voucher, description, visible_after)
            VALUES ($1, $2, $3, (select coalesce(max(visible_after) + '1 second', NOW()) from free_vouchers))
            "#
        )
        .bind(user_id)
        .bind(code)
        .bind(voucher_description)
        .execute(&mut *txn)
        .await
        .context("Failed to insert voucher into database")?;

        created_count += 1;
        // Commit the transaction
        txn.commit().await?;
    }

    Ok(created_count)
}
