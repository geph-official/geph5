use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    net::IpAddr,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use maxminddb::Reader;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use prefix_trie::PrefixSet;

/// List of all Chinese domains.
static DOMAINS: Lazy<HashSet<String>> = Lazy::new(|| {
    let ss = include_str!("china-domains.txt");
    ss.split_ascii_whitespace()
        .filter(|v| v.len() > 1)
        .map(|v| v.to_string())
        .collect()
});

/// Fallback list of Chinese IP subnets (IPv4 only)
static IP_ADDRS: Lazy<PrefixSet<ipnet::Ipv4Net>> = Lazy::new(|| {
    let mut set = PrefixSet::new();
    let ss = include_str!("china-ips.txt");
    for line in ss.lines() {
        set.insert(line.parse().unwrap());
    }
    set
});

// Load the GeoIP database at runtime
static MMDB_READER: Lazy<RwLock<Option<Arc<Reader<Vec<u8>>>>>> = Lazy::new(|| {
    let reader = Reader::open_readfile("Country-only-cn-private.mmdb")
        .map(Arc::new)
        .ok();
    RwLock::new(reader)
});

/// Returns true if the given host is Chinese
pub fn is_chinese_host(host: &str) -> bool {
    if let Ok(ip) = IpAddr::from_str(host) {
        // Try looking up in the dynamic MMDB database first
        let reader_guard = MMDB_READER.read();
        if let Some(reader) = &*reader_guard {
            if let Ok(result) = reader.lookup(ip) {
                if let Ok(Some(country)) = result.decode::<maxminddb::geoip2::Country>() {
                    if let Some(iso_code) = country.country.iso_code {
                        return iso_code.to_uppercase() == "CN";
                    }
                }
            }
        }

        // Fallback to static china-ips.txt list
        if let IpAddr::V4(ipv4) = ip {
            let net = ipnet::Ipv4Net::new(ipv4, 24).unwrap();
            return IP_ADDRS.get_lpm(&net).is_some();
        }
        return false;
    }

    if let Some(host) = psl::domain_str(host) {
        // explode by dots
        let exploded: Vec<_> = host.split('.').collect();
        // join & lookup in loop
        for i in 0..exploded.len() {
            let candidate = (exploded[i..]).join(".");
            if DOMAINS.contains(&candidate) {
                return true;
            }
        }
    }
    false
}

async fn download_db(url: &str) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    
    let response = client.get(url).send().await?;
    if response.status().is_success() {
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    } else {
        anyhow::bail!("Server returned error code: {:?}", response.status())
    }
}

/// Spawns a background task to update the GeoIP database.
pub fn spawn_geoip_updater() {
    geph5_rt::spawn(async {
        let db_path = "Country-only-cn-private.mmdb";
        let temp_path = "Country-only-cn-private.mmdb.tmp";

        // Frequency limit: Check if the file is less than 7 days old
        if let Ok(metadata) = std::fs::metadata(db_path) {
            if let Ok(modified) = metadata.modified() {
                if let Ok(elapsed) = modified.elapsed() {
                    if elapsed.as_secs() < 604_800 {
                        tracing::info!("GeoIP database is fresh (less than 7 days old), skipping update.");
                        return;
                    }
                }
            }
        }

        let github_url = "https://github.com/Loyalsoldier/geoip/releases/latest/download/Country-only-cn-private.mmdb";
        let jsdelivr_url = "https://cdn.jsdelivr.net/gh/Loyalsoldier/geoip@release/Country-only-cn-private.mmdb";

        // Try GitHub first, fallback to jsDelivr CDN on failure
        let download_result = match download_db(github_url).await {
            Ok(bytes) => {
                tracing::info!("GeoIP database downloaded successfully from GitHub.");
                Ok(bytes)
            }
            Err(e) => {
                tracing::warn!("Failed to download from GitHub ({:?}). Retrying via jsDelivr CDN...", e);
                download_db(jsdelivr_url).await
            }
        };

        match download_result {
            Ok(bytes) => {
                let write_result = File::create(temp_path).and_then(|mut file| {
                    file.write_all(&bytes)
                });

                if write_result.is_ok() && std::fs::rename(temp_path, db_path).is_ok() {
                    if let Ok(new_reader) = Reader::open_readfile(db_path) {
                        *MMDB_READER.write() = Some(Arc::new(new_reader));
                        tracing::info!("GeoIP database successfully updated and reloaded!");
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!("GeoIP update failed: All download sources exhausted. Error: {:?}", e);
            }
        }

        // Clean up temp file on failure
        let _ = std::fs::remove_file(temp_path);
    }).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_geoip_updater_and_lookup() {
        let db_path = "Country-only-cn-private.mmdb";
        // Force download by removing existing database file
        let _ = std::fs::remove_file(db_path);

        tracing::info!("Spawning geoip updater...");
        spawn_geoip_updater();

        // Wait up to 30 seconds for download and reload to finish
        let mut success = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if std::fs::metadata(db_path).is_ok() {
                let reader_guard = MMDB_READER.read();
                if reader_guard.is_some() {
                    success = true;
                    break;
                }
            }
        }
        assert!(success, "GeoIP database did not download and load successfully");

        // Test known Chinese IP (Baidu: 220.181.38.148)
        assert!(is_chinese_host("220.181.38.148"));
        
        // Test known foreign IP (Google DNS: 8.8.8.8)
        assert!(!is_chinese_host("8.8.8.8"));

        // Test domain suffix lookups
        assert!(is_chinese_host("baidu.com"));
        assert!(!is_chinese_host("google.com"));

        println!("All GeoIP auto-update and lookup tests passed successfully!");
    }
}
