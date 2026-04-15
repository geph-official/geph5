use std::{
    collections::BTreeMap,
    io::BufRead,
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::Context;
use flate2::bufread::GzDecoder;
use moka::future::Cache;

const IPV4_URL: &str = "https://iptoasn.com/data/ip2asn-v4-u32.tsv.gz";
const IPV6_URL: &str = "https://iptoasn.com/data/ip2asn-v6.tsv.gz";

#[derive(Clone, Debug, PartialEq, Eq)]
struct RangeEntry<K> {
    range_start: K,
    asn: u32,
    country: String,
}

#[derive(Clone, Debug)]
struct IpToAsnMaps {
    ipv4: BTreeMap<u32, RangeEntry<u32>>,
    ipv6: BTreeMap<u128, RangeEntry<u128>>,
}

async fn fetch_map(url: &str) -> anyhow::Result<Vec<u8>> {
    let response = reqwest::get(url).await?;
    Ok(response.bytes().await?.to_vec())
}

fn parse_ipv4_map(reader: impl BufRead) -> anyhow::Result<BTreeMap<u32, RangeEntry<u32>>> {
    parse_map(reader, |s| {
        s.parse::<u32>()
            .with_context(|| format!("invalid IPv4 range bound: {s}"))
    })
}

fn parse_ipv6_map(reader: impl BufRead) -> anyhow::Result<BTreeMap<u128, RangeEntry<u128>>> {
    parse_map(reader, |s| {
        s.parse::<Ipv6Addr>()
            .map(Ipv6Addr::to_bits)
            .with_context(|| format!("invalid IPv6 range bound: {s}"))
    })
}

fn parse_map<K>(
    reader: impl BufRead,
    mut parse_bound: impl FnMut(&str) -> anyhow::Result<K>,
) -> anyhow::Result<BTreeMap<K, RangeEntry<K>>>
where
    K: Ord + Copy,
{
    let mut map = BTreeMap::new();

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }

        let range_start = parse_bound(fields[0])?;
        let range_end = parse_bound(fields[1])?;
        let asn = fields[2].parse()?;

        map.insert(
            range_end,
            RangeEntry {
                range_start,
                asn,
                country: fields[3].to_string(),
            },
        );
    }

    Ok(map)
}

fn lookup_in_map<K>(map: &BTreeMap<K, RangeEntry<K>>, key: K) -> anyhow::Result<(u32, String)>
where
    K: Ord + Copy,
{
    let (_, entry) = map.range(key..).next().context("ASN lookup failed")?;
    if entry.range_start > key {
        anyhow::bail!("ASN lookup failed");
    }
    Ok((entry.asn, entry.country.clone()))
}

async fn get_ip_to_asn_maps() -> anyhow::Result<Arc<IpToAsnMaps>> {
    static CACHE: LazyLock<Cache<(), Arc<IpToAsnMaps>>> = LazyLock::new(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(86400))
            .build()
    });

    CACHE
        .try_get_with((), async move {
            let ipv4_bytes = fetch_map(IPV4_URL).await?;
            let ipv6_bytes = fetch_map(IPV6_URL).await?;

            let ipv4 = parse_ipv4_map(std::io::BufReader::new(GzDecoder::new(&ipv4_bytes[..])))?;
            let ipv6 = parse_ipv6_map(std::io::BufReader::new(GzDecoder::new(&ipv6_bytes[..])))?;

            anyhow::Ok(Arc::new(IpToAsnMaps { ipv4, ipv6 }))
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

pub async fn ip_to_asn_country(ip: IpAddr) -> anyhow::Result<(u32, String)> {
    let maps = get_ip_to_asn_maps().await?;
    match ip {
        IpAddr::V4(ip) => lookup_in_map(&maps.ipv4, ip.to_bits()),
        IpAddr::V6(ip) => lookup_in_map(&maps.ipv6, ip.to_bits()),
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, net::Ipv6Addr};

    use super::*;

    #[test]
    fn parse_ipv4_map_reads_ranges() {
        let data = "\
0\t9\t64500\tZZ\n\
10\t19\t64501\tAA\n";
        let map = parse_ipv4_map(Cursor::new(data)).unwrap();
        assert_eq!(
            map.get(&9),
            Some(&RangeEntry {
                range_start: 0,
                asn: 64500,
                country: "ZZ".into(),
            })
        );
        assert_eq!(
            map.get(&19),
            Some(&RangeEntry {
                range_start: 10,
                asn: 64501,
                country: "AA".into(),
            })
        );
    }

    #[test]
    fn parse_ipv6_map_reads_ranges() {
        let data = "\
2001:db8::\t2001:db8::ffff\t64510\tZZ\n\
2001:db8:1::\t2001:db8:1::ffff\t64511\tAA\n";
        let map = parse_ipv6_map(Cursor::new(data)).unwrap();
        let first_end = "2001:db8::ffff".parse::<Ipv6Addr>().unwrap().to_bits();
        let first_start = "2001:db8::".parse::<Ipv6Addr>().unwrap().to_bits();
        assert_eq!(
            map.get(&first_end),
            Some(&RangeEntry {
                range_start: first_start,
                asn: 64510,
                country: "ZZ".into(),
            })
        );
    }

    #[test]
    fn lookup_in_map_handles_ipv4_boundary() {
        let data = "\
0\t9\t64500\tZZ\n\
10\t19\t64501\tAA\n";
        let map = parse_ipv4_map(Cursor::new(data)).unwrap();
        assert_eq!(lookup_in_map(&map, 9).unwrap(), (64500, "ZZ".into()));
        assert_eq!(lookup_in_map(&map, 10).unwrap(), (64501, "AA".into()));
    }

    #[test]
    fn lookup_in_map_handles_ipv6_boundary() {
        let data = "\
2001:db8::\t2001:db8::ffff\t64510\tZZ\n\
2001:db8:1::\t2001:db8:1::ffff\t64511\tAA\n";
        let map = parse_ipv6_map(Cursor::new(data)).unwrap();
        let first_end = "2001:db8::ffff".parse::<Ipv6Addr>().unwrap().to_bits();
        let second_start = "2001:db8:1::".parse::<Ipv6Addr>().unwrap().to_bits();
        assert_eq!(
            lookup_in_map(&map, first_end).unwrap(),
            (64510, "ZZ".into())
        );
        assert_eq!(
            lookup_in_map(&map, second_start).unwrap(),
            (64511, "AA".into())
        );
    }

    #[test]
    fn lookup_in_map_rejects_gaps() {
        let data = "10\t19\t64501\tAA\n";
        let map = parse_ipv4_map(Cursor::new(data)).unwrap();
        assert!(lookup_in_map(&map, 0).is_err());
    }

    #[test]
    fn lookup_in_map_rejects_empty_map() {
        let map = parse_ipv6_map(Cursor::new("")).unwrap();
        assert!(lookup_in_map(&map, Ipv6Addr::LOCALHOST.to_bits()).is_err());
    }
}
