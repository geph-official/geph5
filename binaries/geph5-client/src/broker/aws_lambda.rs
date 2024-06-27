use anyhow::Context;
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_lambda::{config::Credentials, primitives::Blob};
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};

pub struct AwsLambdaTransport {
    pub function_name: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

#[async_trait]
impl RpcTransport for AwsLambdaTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let client = aws_sdk_lambda::Client::new(
            &aws_config::defaults(BehaviorVersion::v2024_03_28())
                .region(string_to_static_str(self.region.clone()))
                .credentials_provider(Credentials::new(
                    self.access_key_id.clone(),
                    self.secret_access_key.clone(),
                    None,
                    None,
                    "test",
                ))
                .load()
                .await,
        );
        let response = client
            .invoke()
            .function_name(self.function_name.clone())
            .payload(Blob::new(serde_json::to_vec(&req)?))
            .send()
            .await?;
        let blob = response
            .payload()
            .context("empty response from the server")?;
        let resp: JrpcResponse = serde_json::from_slice(blob.as_ref())?;
        Ok(resp)
    }
}

fn string_to_static_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
