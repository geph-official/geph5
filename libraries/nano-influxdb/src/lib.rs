use std::{sync::LazyLock, time::Duration};
use serde::Deserialize;

/// Configuration for an InfluxDB endpoint with authentication details
#[derive(Deserialize, Debug, Clone)]
pub struct InfluxDbEndpoint {
    /// The InfluxDB endpoint URL
    pub url: String,
    /// Username for authentication
    pub username: String,
    /// Password for authentication
    pub password: String,
}

impl InfluxDbEndpoint {
    pub async fn send_line(&self, line: Vec<u8>) -> anyhow::Result<()> {
        static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(1))
                .build()
                .unwrap()
        });

        CLIENT
            .post(&self.url)
            .basic_auth(&self.username, Some(&self.password))
            .body(line)
            .send()
            .await?;
        Ok(())
    }
}