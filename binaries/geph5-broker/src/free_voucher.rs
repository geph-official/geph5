use std::collections::BTreeMap;

use geph5_broker_protocol::VoucherInfo;

use crate::database::POSTGRES;

pub async fn get_free_voucher(user_id: i32) -> anyhow::Result<Option<VoucherInfo>> {
    let row: Option<(String, String)> =
        sqlx::query_as("select voucher,description from free_vouchers where id = $1 limit 1")
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