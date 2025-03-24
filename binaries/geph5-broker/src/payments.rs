use std::time::Duration;

use axum::async_trait;
use nanorpc::{nanorpc_derive, JrpcRequest, JrpcResponse, RpcTransport};
use serde::{Deserialize, Serialize};

use crate::CONFIG_FILE;

pub async fn payment_sessid(user_id: i32) -> anyhow::Result<String> {
    let sessid = PaymentClient(PaymentTransport)
        .raw_start_session(user_id, CONFIG_FILE.wait().payment_support_secret.clone())
        .await?
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(sessid)
}

pub struct PaymentTransport;

#[async_trait]
impl RpcTransport for PaymentTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(client
            .post(&CONFIG_FILE.wait().payment_url)
            .header("content-type", "application/json")
            .body(serde_json::to_string(&req)?)
            .send()
            .await?
            .json()
            .await?)
    }
}

#[nanorpc_derive]
#[async_trait]
pub trait PaymentProtocol {
    async fn raw_start_session(
        &self,
        user_id: i32,
        support_secret: String,
    ) -> Result<String, String>;
    async fn start_stripe_url(
        &self,
        sessid: String,
        args: StartStripeArgs,
    ) -> Result<String, String>;

    async fn start_aliwechat(
        &self,
        sessid: String,
        args: StartAliwechatArgs,
    ) -> Result<String, String>;

    async fn start_crypto(
        &self,
        sessid: String,
        args: StartCryptoArgs,
    ) -> Result<serde_json::Value, String>;

    async fn create_giftcard(&self, support_secret: String, days: i32) -> Result<String, String>;

    async fn spend_giftcard(&self, sessid: String, info: GiftcardWireInfo) -> Result<i32, String>;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct GiftcardWireInfo {
    pub gc_id: String,
    pub promo: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StartAliwechatArgs {
    pub promo: String,
    pub days: i32,
    pub item: Item,
    pub method: String,
    pub mobile: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StartStripeArgs {
    pub promo: String,
    pub days: i32,
    pub item: Item,
    pub is_recurring: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StartCryptoArgs {
    pub promo: String,
    pub days: i32,
    pub item: Item,
    pub token: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Item {
    Plus,
    Giftcard(GiftcardInfo),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GiftcardInfo {
    pub recipient_email: String,
    pub sender: String,
    pub count: u32,
}
