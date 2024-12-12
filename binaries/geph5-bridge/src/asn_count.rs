use anyhow::Context;
use dashmap::DashMap;
use flate2::read::GzDecoder;
use moka::future::Cache;
use once_cell::sync::Lazy;
use std::{collections::BTreeMap, io::BufRead, net::IpAddr, sync::LazyLock, time::Duration};

pub static ASN_CONN_COUNT: Lazy<DashMap<u32, u32>> = Lazy::new(|| DashMap::new());
pub async fn ip_to_asn(ip: IpAddr) -> anyhow::Result<u32> {
    let ip_to_asn_map = get_ip_to_asn_map().await?;
    let ip = match ip {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(_) => return Err(anyhow::anyhow!("IPv6 not supported")),
    };
    let (_, (asn, _country)) = ip_to_asn_map
        .range(ip.to_bits()..)
        .next()
        .context("ASN lookup failed")?;
    Ok(*asn)
}

// Increment the connection count for a given ASN
pub fn incr_asn_conn_count(asn: u32) -> u32 {
    let mut entry = ASN_CONN_COUNT.entry(asn).or_insert(0);
    *entry += 1;
    *entry
}

pub fn decr_asn_conn_count(asn: u32) -> anyhow::Result<u32> {
    let mut entry = ASN_CONN_COUNT.get_mut(&asn).context("no such asn")?;
    *entry -= 1;
    Ok(*entry)
}

async fn get_ip_to_asn_map() -> anyhow::Result<BTreeMap<u32, (u32, String)>> {
    static ASN_MAP_CACHE: LazyLock<Cache<String, BTreeMap<u32, (u32, String)>>> =
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
            anyhow::Ok(map)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
}
