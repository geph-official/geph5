use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_lambda::{config::Credentials, primitives::Blob};
use aws_smithy_runtime::client::http::hyper_014::HyperClientBuilder;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use serde::Deserialize;

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
        tracing::debug!(method = req.method, "calling broker through lambda");
        let start = Instant::now();
        // To use webpki-roots we need to create a custom http client, as explained here: https://github.com/smithy-lang/smithy-rs/discussions/3022

        // Create a connector that will be used to establish TLS connections
        let tls_connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();

        // Create a hyper-based HTTP client that uses this TLS connector.
        let http_client = HyperClientBuilder::new().build(tls_connector);

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
                .http_client(http_client)
                .load()
                .await,
        );
        let response = client
            .invoke()
            .function_name(self.function_name.clone())
            .payload(Blob::new(serde_json::to_string(&req)?))
            .send()
            .await
            .context("lambda function failed")?;
        let blob = response
            .payload()
            .context("empty response from the server")?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            status_code: usize,
            body: String,
        }
        tracing::debug!(
            method = req.method,
            resp_len = blob.as_ref().len(),
            elapsed = debug(start.elapsed()),
            "response received through lambda"
        );
        let resp: Response = serde_json::from_slice(blob.as_ref())?;
        if resp.status_code == 200 {
            Ok(serde_json::from_str(&resp.body)?)
        } else {
            anyhow::bail!("error code {}, body {:?}", resp.status_code, resp.body)
        }
    }
}

fn string_to_static_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
