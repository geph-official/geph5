use minisign_verify::{PublicKey, Signature};
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

    // Download the metadata.yaml file
    let yaml_bytes = GET_MANIFEST_CLIENT
        .get(format!("{base_url}/metadata.yaml"))
        .send()
        .await?
        .bytes()
        .await?;

    // Download the signature file
    let signature_str = GET_MANIFEST_CLIENT
        .get(format!("{base_url}/metadata.yaml.minisig"))
        .send()
        .await?
        .text()
        .await?;

    // Verify the signature
    // Hardcoded public key
    let public_key =
        PublicKey::from_base64("RWSzEWRCN0AaNpPj+yw0zbOI87jI8PNpnCoITCroKQxRAANAzUawpph7")
            .map_err(|_| anyhow::anyhow!("Unable to decode the public key"))?;

    let signature = Signature::decode(&signature_str)
        .map_err(|_| anyhow::anyhow!("Unable to decode the signature"))?;

    // Verify the signature on the raw YAML bytes
    public_key
        .verify(&yaml_bytes, &signature, true)
        .map_err(|_| anyhow::anyhow!("Signature verification failed"))?;

    // Parse the YAML after verification
    let val: serde_json::Value = serde_yaml::from_slice(&yaml_bytes)?;

    Ok(val)
}
