use std::{sync::LazyLock, time::Duration};

pub async fn get_update_manifest() -> anyhow::Result<(serde_json::Value, String)> {
    let urls = [
        "https://pub-a02e86bb722a4b079127aeee6d399e5d.r2.dev/geph-releases",
        "https://f001.backblazeb2.com/file/geph4-dl/geph-releases",
    ];
    for url in urls {
        match get_update_manifest_inner(url).await {
            Ok(ret) => return Ok((ret, url.to_string())),
            Err(err) => {
                tracing::warn!(url, err = debug(err), "skipping bad url for manifest")
            }
        }
    }
    anyhow::bail!("no urls worked for manifest")
}

async fn get_update_manifest_inner(base_url: &str) -> anyhow::Result<serde_json::Value> {
    static GET_MANIFEST_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap()
    });
    let val: serde_json::Value = serde_yaml::from_slice(
        &GET_MANIFEST_CLIENT
            .get(format!("{base_url}/metadata.yaml"))
            .send()
            .await?
            .bytes()
            .await?,
    )?;
    Ok(val)
}
