use dashmap::DashMap;
use geph5_ip_to_asn::ip_to_asn_country;
use influxdb_line_protocol::LineProtocolBuilder;

use std::{net::IpAddr, sync::LazyLock};

use crate::influxdb::INFLUXDB_ENDPOINT;

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
    if INFLUXDB_ENDPOINT.is_none() {
        return;
    }

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

// Flush the byte count for a specific pool+ASN.
fn flush_asn_data((pool, asn): (String, u32)) {
    if let Some(val) = &*INFLUXDB_ENDPOINT
        && let Some((_, bytes)) = ASN_BYTE_COUNTS.remove(&(pool.clone(), asn))
    {
        let endpoint = val.clone();
        let country_code = ASN_COUNTRY_MAP
            .get(&asn)
            .map(|c| c.clone())
            .unwrap_or_else(|| "XX".to_string());

        smolscale::spawn(async move {
            let _ = endpoint
                .send_line(
                    LineProtocolBuilder::new()
                        .measurement("bridge_bytes")
                        .tag("pool", &pool)
                        .tag("asn", &asn.to_string())
                        .tag("country", &country_code)
                        .field("bytes", bytes as f64)
                        .close_line()
                        .build(),
                )
                .await;
        })
        .detach();
    }
}
