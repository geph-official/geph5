use std::collections::HashSet;

use once_cell::sync::Lazy;

/// List of all Chinese domains.
static DOMAINS: Lazy<HashSet<String>> = Lazy::new(|| {
    let ss = include_str!("china-domains.txt");
    ss.split_ascii_whitespace()
        .filter(|v| v.len() > 1)
        .map(|v| v.to_string())
        .collect()
});

/// Returns true if the given host is Chinese
pub fn is_chinese_host(host: &str) -> bool {
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
