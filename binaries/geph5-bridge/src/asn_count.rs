use dashmap::DashMap;
use geph5_ip_to_asn::ip_to_asn_country;

use std::{net::IpAddr, sync::LazyLock};

use crate::stats::STAT_BATCHER;

// Global mapping of ASN to byte counts
static ASN_BYTE_COUNTS: LazyLock<DashMap<(String, u32), u64>> = LazyLock::new(DashMap::new);
// Global mapping of ASN to country code
static ASN_COUNTRY_MAP: LazyLock<DashMap<u32, String>> = LazyLock::new(DashMap::new);

const FLUSH_THRESHOLD: u64 = 1_000_000;

pub async fn ip_to_asn(ip: IpAddr) -> anyhow::Result<u32> {
    let (asn, country) = ip_to_asn_country(ip).await?;
    ASN_COUNTRY_MAP.insert(asn, country);
    Ok(asn)
}

// Increment the byte count for a given pool+ASN and flush if threshold is reached.
pub fn incr_bytes_asn(pool: &str, asn: u32, bytes: u64) {
    let key = (pool.to_string(), asn);

    let should_flush = {
        let mut entry = ASN_BYTE_COUNTS.entry(key.clone()).or_insert(0);
        *entry += bytes;
        *entry >= FLUSH_THRESHOLD
    };

    if should_flush {
        flush_asn_data(key);
    }
}

// Flush the byte count for a specific pool+ASN into the stats batcher.
fn flush_asn_data((pool, asn): (String, u32)) {
    if let Some((_, bytes)) = ASN_BYTE_COUNTS.remove(&(pool.clone(), asn)) {
        let country_code = ASN_COUNTRY_MAP
            .get(&asn)
            .map(|c| c.clone())
            .unwrap_or_else(|| "XX".to_string());

        STAT_BATCHER.counter(
            "bridge_bytes",
            &[
                ("pool", &pool),
                ("asn", &asn.to_string()),
                ("country", &country_code),
            ],
            bytes as f64,
        );
    }
}
