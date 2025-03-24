use std::collections::BTreeMap;

use geph5_broker_protocol::VoucherInfo;

use crate::database::POSTGRES;

pub async fn get_free_voucher(user_id: i32) -> anyhow::Result<Option<VoucherInfo>> {
    let row: Option<(String, String)> =
        sqlx::query_as("select voucher,description from free_voucher where id = $1 limit 1")
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
