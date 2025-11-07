use anyctx::AnyCtx;
use minisign_verify::{PublicKey, Signature};
use std::{sync::LazyLock, time::Duration};

use crate::{broker::broker_client, client::Config};

pub async fn get_update_manifest(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<(serde_json::Value, String)> {
    if let Some(bundle) = get_manifest_via_broker(ctx).await? {
        return Ok(bundle);
    }

    let urls = [
        "https://f001.backblazeb2.com/file/geph4-dl/geph-releases",
        "https://sos-ch-dk-2.exo.io/utopia/geph-releases-new",
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

async fn get_manifest_via_broker(
    ctx: &AnyCtx<Config>,
) -> anyhow::Result<Option<(serde_json::Value, String)>> {
    let client = match broker_client(ctx) {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!(
                err = debug(err),
                "broker client unavailable for update manifest"
            );
            return Ok(None);
        }
    };
    let manifest = match client.get_update_manifest().await {
        Ok(Ok(bundle)) => {
            let yaml_bytes = bundle.manifest_yaml.into_bytes();
            parse_manifest(&yaml_bytes, &bundle.signature)?
        }
        Ok(Err(err)) => {
            tracing::warn!(err = %err.0, "broker refused to serve update manifest");
            return Ok(None);
        }
        Err(err) => {
            tracing::warn!(err = debug(err), "broker RPC failed for update manifest");
            return Ok(None);
        }
    };
    Ok(Some((manifest, "broker".to_string())))
}

async fn get_update_manifest_inner(base_url: &str) -> anyhow::Result<serde_json::Value> {
    let (yaml_bytes, signature_str) = download_manifest_from_url(base_url).await?;
    parse_manifest(&yaml_bytes, &signature_str)
}

fn parse_manifest(yaml_bytes: &[u8], signature_str: &str) -> anyhow::Result<serde_json::Value> {
    // Hardcoded public key
    let public_key =
        PublicKey::from_base64("RWSzEWRCN0AaNpPj+yw0zbOI87jI8PNpnCoITCroKQxRAANAzUawpph7")
            .map_err(|_| anyhow::anyhow!("Unable to decode the public key"))?;

    let signature = Signature::decode(signature_str)
        .map_err(|_| anyhow::anyhow!("Unable to decode the signature"))?;

    public_key
        .verify(yaml_bytes, &signature, true)
        .map_err(|_| anyhow::anyhow!("Signature verification failed"))?;

    let val: serde_json::Value = serde_yaml::from_slice(yaml_bytes)?;

    Ok(val)
}

async fn download_manifest_from_url(base_url: &str) -> anyhow::Result<(Vec<u8>, String)> {
    static GET_MANIFEST_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap()
    });

    let yaml_bytes = GET_MANIFEST_CLIENT
        .get(format!("{base_url}/metadata.yaml"))
        .send()
        .await?
        .bytes()
        .await?
        .to_vec();

    let signature_str = GET_MANIFEST_CLIENT
        .get(format!("{base_url}/metadata.yaml.minisig"))
        .send()
        .await?
        .text()
        .await?;

    Ok((yaml_bytes, signature_str))
}
