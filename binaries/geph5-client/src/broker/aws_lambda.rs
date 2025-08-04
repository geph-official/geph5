#![cfg(not(target_os = "ios"))]

use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use aws_config::{timeout::TimeoutConfig, BehaviorVersion};
use aws_sdk_lambda::{config::Credentials, primitives::Blob};
use aws_smithy_runtime::client::http::hyper_014::HyperClientBuilder;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use serde::Deserialize;

pub struct AwsLambdaTransport {
    pub function_name: String,
    pub region: String,
    pub obfs_key: String,
}

#[async_trait]
impl RpcTransport for AwsLambdaTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let (access_key_id_b32, secret_access_key_b32) =
            self.obfs_key.split_once(':').context("cannot split")?;
        let access_key_id = String::from_utf8_lossy(
            &base32::decode(base32::Alphabet::Crockford, access_key_id_b32)
                .context("cannot decode access key")?,
        )
        .to_string();
        let secret_access_key = String::from_utf8_lossy(
            &base32::decode(base32::Alphabet::Crockford, secret_access_key_b32)
                .context("cannot decode secret access key")?,
        )
        .to_string();

        tracing::debug!(method = req.method, "calling broker through lambda");
        let start = Instant::now();
        // To use webpki-roots we need to create a custom http client, as explained here: https://github.com/smithy-lang/smithy-rs/discussions/3022

        // Create a connector that will be used to establish TLS connections
        let tls_connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .enable_http2()
            .build();

        // Create a hyper-based HTTP client that uses this TLS connector.
        let http_client = HyperClientBuilder::new().build(tls_connector);

        let client = aws_sdk_lambda::Client::new(
            &aws_config::defaults(BehaviorVersion::v2024_03_28())
                .region(string_to_static_str(self.region.clone()))
                .credentials_provider(Credentials::new(
                    access_key_id,
                    secret_access_key,
                    None,
                    None,
                    "test",
                ))
                .http_client(http_client)
                .timeout_config(
                    TimeoutConfig::builder()
                        .operation_timeout(Duration::from_secs(10))
                        .build(),
                )
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
