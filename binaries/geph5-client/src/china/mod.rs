use std::{collections::HashSet, net::Ipv4Addr, str::FromStr};

use once_cell::sync::Lazy;
use prefix_trie::PrefixSet;

/// List of all Chinese domains.
static DOMAINS: Lazy<HashSet<String>> = Lazy::new(|| {
    let ss = include_str!("china-domains.txt");
    ss.split_ascii_whitespace()
        .filter(|v| v.len() > 1)
        .map(|v| v.to_string())
        .collect()
});

/// List of all Chinese domains.
static IP_ADDRS: Lazy<PrefixSet<ipnet::Ipv4Net>> = Lazy::new(|| {
    let mut set = PrefixSet::new();
    let ss = include_str!("china-ips.txt");
    for line in ss.lines() {
        set.insert(line.parse().unwrap());
    }
    set
});

/// Returns true if the given host is Chinese
pub fn is_chinese_host(host: &str) -> bool {
    if let Ok(ipv4) = Ipv4Addr::from_str(host) {
        let net = ipnet::Ipv4Net::new(ipv4, 24).unwrap();
        return IP_ADDRS.contains(&net);
    }
    // explode by dots
    let exploded: Vec<_> = host.split('.').collect();
    // join & lookup in loop
    for i in 0..exploded.len() {
        let candidate = (exploded[i..]).join(".");
        if DOMAINS.contains(&candidate) {
            return true;
        }
    }
    false
}
