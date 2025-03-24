use anyhow::Context;
use dashmap::DashMap;
use flate2::read::GzDecoder;
use influxdb_line_protocol::LineProtocolBuilder;
use moka::future::Cache;

use std::{
    collections::BTreeMap,
    io::BufRead,
    net::IpAddr,
    sync::{Arc, LazyLock},
    time::Duration,
};

use crate::influxdb::INFLUXDB_ENDPOINT;

// Global mapping of ASN to byte counts
static ASN_BYTE_COUNTS: LazyLock<DashMap<u32, u64>> = LazyLock::new(DashMap::new);
// Global mapping of ASN to country code
static ASN_COUNTRY_MAP: LazyLock<DashMap<u32, String>> = LazyLock::new(DashMap::new);

const FLUSH_THRESHOLD: u64 = 1_000_000;

pub async fn ip_to_asn(ip: IpAddr) -> anyhow::Result<u32> {
    let ip_to_asn_map = get_ip_to_asn_map().await?;
    let ip = match ip {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(_) => return Err(anyhow::anyhow!("IPv6 not supported")),
    };
    let (_, (asn, country)) = ip_to_asn_map
        .range(ip.to_bits()..)
        .next()
        .context("ASN lookup failed")?;

    // Store the ASN to country mapping
    ASN_COUNTRY_MAP.insert(*asn, country.clone());

    Ok(*asn)
}

// Increment the byte count for a given ASN and flush if threshold is reached
pub fn incr_bytes_asn(asn: u32, bytes: u64) {
    if INFLUXDB_ENDPOINT.is_none() {
        return;
    }

    // Update the counter in the DashMap
    let should_flush = {
        let mut entry = ASN_BYTE_COUNTS.entry(asn).or_insert(0);
        *entry += bytes;
        *entry >= FLUSH_THRESHOLD
    };

    // If we've reached the threshold, flush this ASN's data
    if should_flush {
        flush_asn_data(asn);
    }
}

// Flush the byte count for a specific ASN
fn flush_asn_data(asn: u32) {
    if let Some(val) = &*INFLUXDB_ENDPOINT {
        // Only proceed if we can remove the entry (preventing concurrent flushes)
        if let Some((_, bytes)) = ASN_BYTE_COUNTS.remove(&asn) {
            // Clone the endpoint for the async block
            let endpoint = val.clone();

            // Get the country code from our mapping
            let country_code = ASN_COUNTRY_MAP
                .get(&asn)
                .map(|c| c.clone())
                .unwrap_or_else(|| "XX".to_string());

            smolscale::spawn(async move {
                let pool = match std::env::var("GEPH5_BRIDGE_POOL") {
                    Ok(p) => p,
                    Err(_) => return, // Skip sending if pool is not set
                };

                let _ = endpoint
                    .send_line(
                        LineProtocolBuilder::new()
                            .measurement("bridge_bytes")
                            .tag("pool", &pool)
                            .tag("asn", &asn.to_string())
                            .tag("country", &country_code) // Add country code tag
                            .field("bytes", bytes as f64)
                            .close_line()
                            .build(),
                    )
                    .await;
            })
            .detach();
        }
    }
}

async fn get_ip_to_asn_map() -> anyhow::Result<Arc<BTreeMap<u32, (u32, String)>>> {
    static ASN_MAP_CACHE: LazyLock<Cache<String, Arc<BTreeMap<u32, (u32, String)>>>> =
        LazyLock::new(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(86400))
                .build()
        });

    ASN_MAP_CACHE
        .try_get_with("key".to_string(), async {
            let url = "https://iptoasn.com/data/ip2asn-v4-u32.tsv.gz";
            let response = reqwest::get(url).await?;
            let bytes = response.bytes().await?;

            let decoder = GzDecoder::new(&bytes[..]);
            let reader = std::io::BufReader::new(decoder);

            let mut map = BTreeMap::new();

            for line in reader.lines() {
                let line = line?;
                let fields: Vec<&str> = line.split('\t').collect();

                if fields.len() >= 4 {
                    let range_end: u32 = fields[1].parse()?;
                    let as_number: u32 = fields[2].parse()?;
                    let country_code = fields[3].to_string();

                    map.insert(range_end, (as_number, country_code));
                }
            }
            anyhow::Ok(Arc::new(map))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
}
