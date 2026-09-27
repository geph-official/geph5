use std::{
    sync::LazyLock,
    time::{Instant, SystemTime},
};

use anyhow::Context;
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{
    Client, Request, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;

use super::http_client::{client_builder, egress_addrs, with_dns};

pub struct AwsLambdaTransport {
    pub function_name: String,
    pub region: String,
    pub obfs_key: String,
}

// AWS path labels escape everything except RFC 3986 unreserved characters.
const PATH_LABEL: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn invoke_url(region: &str, function_name: &str) -> anyhow::Result<Url> {
    anyhow::ensure!(
        !region.is_empty()
            && region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid Lambda region"
    );
    anyhow::ensure!(!function_name.is_empty(), "empty Lambda function name");
    Url::parse(&format!(
        "https://lambda.{region}.amazonaws.com/2015-03-31/functions/{}/invocations",
        utf8_percent_encode(function_name, PATH_LABEL),
    ))
    .context("invalid Lambda Invoke URL")
}

fn credentials(obfs_key: &str) -> anyhow::Result<Credentials> {
    let (id, secret) = obfs_key
        .split_once(':')
        .context("cannot split Lambda credentials")?;
    let decode = |value, label| -> anyhow::Result<String> {
        let bytes = base32::decode(base32::Alphabet::Crockford, value).context(label)?;
        Ok(String::from_utf8(bytes).context("Lambda credentials are not UTF-8")?)
    };
    Ok(Credentials::new(
        decode(id, "cannot decode access key")?,
        decode(secret, "cannot decode secret access key")?,
        None,
        None,
        "geph-lambda",
    ))
}

fn lambda_client_builder() -> reqwest::ClientBuilder {
    // A redirect changes the signed authority/path. Retry behavior stays at
    // reqwest's default; RPC retries and deadlines belong to the broker layer.
    client_builder().redirect(reqwest::redirect::Policy::none())
}

fn signed_request(
    client: &Client,
    url: Url,
    body: Vec<u8>,
    credentials: &Credentials,
    region: &str,
    time: SystemTime,
) -> anyhow::Result<Request> {
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-amz-invocation-type", "RequestResponse")
        .body(body)
        .build()?;
    let identity = credentials.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("lambda")
        .time(time)
        .settings(SigningSettings::default())
        .build()?
        .into();
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| Ok((name.as_str(), value.to_str()?)))
        .collect::<Result<Vec<_>, reqwest::header::ToStrError>>()?;
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.url().as_str(),
        headers.into_iter(),
        SignableBody::Bytes(
            request
                .body()
                .and_then(|b| b.as_bytes())
                .context("missing Lambda request body")?,
        ),
    )?;
    let (instructions, _) = sign(signable, &params)?.into_parts();
    let (headers, _) = instructions.into_parts();
    for header in headers {
        let mut value = HeaderValue::from_str(header.value())?;
        value.set_sensitive(header.sensitive() || header.name() == "authorization");
        request
            .headers_mut()
            .insert(HeaderName::from_static(header.name()), value);
    }
    Ok(request)
}

async fn invoke(client: &Client, request: Request) -> anyhow::Result<JrpcResponse> {
    let response = client
        .execute(request)
        .await
        .context("cannot send Lambda Invoke request")?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .context("cannot read Lambda response")?;
    decode_response(status, &headers, &body)
}

fn decode_response(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> anyhow::Result<JrpcResponse> {
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "Lambda Invoke HTTP error {status}: {}",
        String::from_utf8_lossy(body)
    );
    anyhow::ensure!(
        !headers.contains_key("x-amz-function-error"),
        "Lambda function failed: {}",
        String::from_utf8_lossy(body)
    );
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Response {
        status_code: usize,
        body: String,
    }
    let response: Response =
        serde_json::from_slice(body).context("invalid Lambda response envelope")?;
    anyhow::ensure!(
        response.status_code == 200,
        "error code {}, body {:?}",
        response.status_code,
        response.body
    );
    serde_json::from_str(&response.body).context("invalid Lambda broker response")
}

#[async_trait]
impl RpcTransport for AwsLambdaTransport {
    type Error = anyhow::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        static POOL: LazyLock<Client> = LazyLock::new(|| lambda_client_builder().build().unwrap());
        tracing::trace!(method = req.method, "calling broker through lambda");
        let start = Instant::now();
        let credentials = credentials(&self.obfs_key)?;
        let url = invoke_url(&self.region, &self.function_name)?;
        let client = match egress_addrs(&url, None).await? {
            Some(addrs) => with_dns(lambda_client_builder(), addrs)
                .build()
                .context("could not build bound Lambda client")?,
            None => POOL.clone(),
        };
        // Sign the original AWS URL and exactly the bytes sent, after physical
        // DNS resolution. The loopback forwarder changes only the TCP route.
        let request = signed_request(
            &client,
            url,
            serde_json::to_vec(&req)?,
            &credentials,
            &self.region,
            SystemTime::now(),
        )?;
        let response = invoke(&client, request).await?;
        tracing::trace!(
            method = req.method,
            elapsed = debug(start.elapsed()),
            "response received through lambda"
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PAYLOAD: &[u8] = br#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#;
    const RESPONSE: &str =
        r#"{"statusCode":200,"body":"{\"jsonrpc\":\"2.0\",\"result\":\"ok\",\"id\":1}"}"#;

    fn example_credentials() -> Credentials {
        Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        )
    }

    #[test]
    fn signs_exact_body_and_encoded_arn() {
        let url = invoke_url(
            "us-east-1",
            "arn:aws:lambda:us-east-1:123456789012:function:broker:live",
        )
        .unwrap();
        assert_eq!(
            url.path(),
            "/2015-03-31/functions/arn%3Aaws%3Alambda%3Aus-east-1%3A123456789012%3Afunction%3Abroker%3Alive/invocations"
        );
        let request = signed_request(
            &lambda_client_builder().build().unwrap(),
            url,
            PAYLOAD.to_vec(),
            &example_credentials(),
            "us-east-1",
            SystemTime::UNIX_EPOCH + Duration::from_secs(1704164645),
        )
        .unwrap();
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), PAYLOAD);
        assert_eq!(request.headers()["x-amz-date"], "20240102T030405Z");
        // Independently computed with Python hashlib/hmac from the AWS SigV4
        // canonical request, including double encoding of the ARN path label.
        assert_eq!(
            request.headers()["authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240102/us-east-1/lambda/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-invocation-type, Signature=c615ddd3eaffd953bb88e4d7239b9787aee0d6d596a45a4e5e102867cc6cf757"
        );
        assert!(request.headers()["authorization"].is_sensitive());
        assert!(
            invoke_url("us-east-1", "broker:$LATEST")
                .unwrap()
                .path()
                .contains("broker%3A%24LATEST")
        );
        assert!(invoke_url("us-east-1", "").is_err());
        assert!(invoke_url("example.com/path", "broker").is_err());
    }

    #[test]
    fn decodes_existing_credentials_and_response_envelope() {
        let key = format!(
            "{}:{}",
            base32::encode(base32::Alphabet::Crockford, b"AKIDEXAMPLE"),
            base32::encode(base32::Alphabet::Crockford, b"secret")
        );
        let creds = credentials(&key).unwrap();
        assert_eq!(creds.access_key_id(), "AKIDEXAMPLE");
        assert_eq!(creds.secret_access_key(), "secret");
        assert!(credentials("invalid").is_err());
        let result = decode_response(
            reqwest::StatusCode::OK,
            &HeaderMap::new(),
            RESPONSE.as_bytes(),
        )
        .unwrap();
        assert_eq!(result.result, Some(serde_json::json!("ok")));
        for body in [
            b"".as_slice(),
            b"{}",
            br#"{"statusCode":500,"body":"failed"}"#,
            br#"{"statusCode":200,"body":"invalid"}"#,
        ] {
            assert!(decode_response(reqwest::StatusCode::OK, &HeaderMap::new(), body).is_err());
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-function-error",
            HeaderValue::from_static("Unhandled"),
        );
        assert!(
            decode_response(reqwest::StatusCode::OK, &headers, RESPONSE.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("Lambda function failed")
        );
    }

    #[tokio::test]
    async fn sends_signed_request_and_returns_http_errors_without_a_retry_loop() {
        for status in [200, 403, 429, 500, 302] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        headers.push(stream.read_u8().await.unwrap());
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
                    assert!(
                        headers.starts_with(
                            "post /2015-03-31/functions/broker/invocations http/1.1\r\n"
                        )
                    );
                    assert!(headers.contains("\r\nhost: lambda.us-east-1.amazonaws.com\r\n"));
                    assert!(headers.contains("\r\nauthorization: aws4-hmac-sha256 "));
                    let mut body = vec![0; PAYLOAD.len()];
                    stream.read_exact(&mut body).await.unwrap();
                    assert_eq!(body, PAYLOAD);
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nLocation: /redirected\r\nConnection: close\r\n\r\n{RESPONSE}",
                        RESPONSE.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });
            // Use plain HTTP solely for this local wire-format test. Production
            // URLs are always HTTPS; shared egress has a separate TLS test.
            let client = with_dns(lambda_client_builder(), vec![addr])
                .build()
                .unwrap();
            let mut url = invoke_url("us-east-1", "broker").unwrap();
            url.set_scheme("http").unwrap();
            let request = signed_request(
                &client,
                url,
                PAYLOAD.to_vec(),
                &example_credentials(),
                "us-east-1",
                SystemTime::now(),
            )
            .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), invoke(&client, request))
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.is_ok(), status == 200);
            if let Err(error) = result {
                assert!(error.to_string().contains(&status.to_string()), "{error}");
            }
            assert!(!server.is_finished(), "mock server failed");
            server.abort();
        }
    }
}
