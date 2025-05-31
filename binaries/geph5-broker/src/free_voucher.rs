use std::collections::BTreeMap;

use geph5_broker_protocol::VoucherInfo;

use crate::{
    database::POSTGRES,
    payments::{PaymentClient, PaymentTransport},
    CONFIG_FILE,
};

pub async fn get_free_voucher(user_id: i32) -> anyhow::Result<Option<VoucherInfo>> {
    loop {
        let mut txn = POSTGRES.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
        "select id, voucher,description from free_vouchers natural join users where id = $1 limit 1",
    )
    .bind(user_id)
    .fetch_optional(&mut *txn)
    .await?;
        if let Some((voucher, description)) = row {
            if voucher == "" {
                // dynamically generate one and save
                // HACK: using the description to uniquely identify the row here.
                let code = PaymentClient(PaymentTransport)
                    .create_giftcard(CONFIG_FILE.wait().payment_support_secret.clone(), 1)
                    .await?
                    .map_err(|e| anyhow::anyhow!(e))?;
                sqlx::query(
                    "update free_vouchers set voucher = $1 where id = $2 and description = $3",
                )
                .bind(code)
                .bind(user_id)
                .bind(description)
                .execute(&mut *txn)
                .await?;
                txn.commit().await?;
            } else {
                let map: BTreeMap<String, String> = serde_json::from_str(&description)?;
                return Ok(Some(VoucherInfo {
                    code: voucher,
                    explanation: map,
                }));
            }
        } else {
            return Ok(None);
        }
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
