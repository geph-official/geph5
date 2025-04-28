use std::{
    collections::BTreeMap,
    io::BufRead,
    net::Ipv4Addr,
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::Context;
use flate2::bufread::GzDecoder;
use moka::future::Cache;

async fn get_ip_to_asn_map() -> anyhow::Result<BTreeMap<u32, (u32, String)>> {
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

    Ok(map)
}

static CACHE: LazyLock<Cache<(), Arc<BTreeMap<u32, (u32, String)>>>> = LazyLock::new(|| {
    Cache::builder()
        .time_to_live(Duration::from_secs(86400))
        .build()
});

pub async fn ip_to_asn_country(ip: Ipv4Addr) -> anyhow::Result<(u32, String)> {
    let ip_to_asn = CACHE
        .try_get_with((), async move {
            let v = get_ip_to_asn_map().await?;
            anyhow::Ok(Arc::new(v))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let (_, (asn, country)) = ip_to_asn
        .range(ip.to_bits()..)
        .next()
        .context("ASN lookup failed")?;
    Ok((*asn, country.clone()))
}
