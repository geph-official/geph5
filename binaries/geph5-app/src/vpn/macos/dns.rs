//! Sentinel DNS via SCDynamicStore, in the style of Mullvad's talpid-dns:
//! overwrite each service's DNS in BOTH `State:/Network/Service/<id>/DNS` and
//! the matching `Setup:` key, and put the priors back on teardown.
//!
//! Both keys matter. A `State:`-only override is attributed to the service's
//! physical interface, so mDNSResponder sends interface-scoped queries that
//! bypass the tun's `/1` routes and die in the kill switch's port-53 block
//! (observed live: 52 dropped queries while `dig` through the tun worked). The
//! `Setup:` override is what makes the sentinel the *default, unscoped*
//! resolver, and configd re-derives `State:` from it.

use system_configuration::{
    core_foundation::{
        array::CFArray,
        base::{CFType, TCFType},
        dictionary::CFDictionary,
        string::CFString,
    },
    dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder},
};

const STATE_DNS_PATTERN: &str = "State:/Network/Service/.*/DNS";
const SERVER_ADDRESSES: &str = "ServerAddresses";

/// Per touched key (State and Setup alike): its path and the servers to put
/// back. `None` = the key had no prior override (or carried our sentinels from
/// a crashed instance) — remove it and let configd re-derive.
pub type DnsBackup = Vec<(String, Option<Vec<String>>)>;

fn store() -> SCDynamicStore {
    SCDynamicStoreBuilder::new("io.geph.manager.dns").build()
}

/// `State:/Network/Service/<id>/DNS` → the service's `Setup:` DNS key.
fn state_to_setup_path(state_path: &str) -> Option<String> {
    state_path
        .strip_prefix("State:/")
        .map(|rest| format!("Setup:/{rest}"))
}

/// Every service's State DNS path plus its Setup twin.
fn all_dns_paths(store: &SCDynamicStore) -> Vec<String> {
    let mut paths: Vec<String> = store
        .get_keys(STATE_DNS_PATTERN)
        .map(|keys| keys.iter().map(|k| k.to_string()).collect())
        .unwrap_or_default();
    let setups: Vec<String> = paths
        .iter()
        .filter_map(|p| state_to_setup_path(p))
        .collect();
    paths.extend(setups);
    paths
}

fn server_addresses(store: &SCDynamicStore, path: &str) -> Option<Vec<String>> {
    let dict = store
        .get(CFString::new(path))?
        .downcast_into::<CFDictionary>()?;
    let v = dict.find(CFString::new(SERVER_ADDRESSES).as_CFType().as_CFTypeRef() as *const _)?;
    let arr = unsafe { CFArray::<CFString>::wrap_under_get_rule(*v as *const _) };
    Some(arr.iter().map(|s| s.to_string()).collect())
}

fn dns_dict(servers: &[&str]) -> CFDictionary<CFString, CFType> {
    let addrs = CFArray::from_CFTypes(
        &servers
            .iter()
            .map(|s| CFString::new(s))
            .collect::<Vec<_>>(),
    );
    CFDictionary::from_CFType_pairs(&[(CFString::new(SERVER_ADDRESSES), addrs.as_CFType())])
}

fn set_servers(store: &SCDynamicStore, path: &str, servers: &[&str]) -> bool {
    store.set(CFString::new(path), dns_dict(servers).to_untyped())
}

/// Point every service at the sentinels (State + Setup), returning what
/// restore needs.
pub fn set_sentinel(v4: &str, v6: &str) -> DnsBackup {
    let store = store();
    let mut backup = Vec::new();
    for path in all_dns_paths(&store) {
        let prior = server_addresses(&store, &path)
            .filter(|servers| !servers.iter().any(|s| s == v4 || s == v6));
        set_servers(&store, &path, &[v4, v6]);
        backup.push((path, prior));
    }
    backup
}

/// Re-apply the sentinels (configd may have republished a service's real DNS
/// after a network event) without touching the backup.
pub fn reassert_sentinel(v4: &str, v6: &str) {
    let store = store();
    for path in all_dns_paths(&store) {
        set_servers(&store, &path, &[v4, v6]);
    }
}

/// Put every touched key back: prior servers if we recorded them, otherwise
/// remove the key so configd re-derives it.
pub fn restore(backup: &DnsBackup) {
    let store = store();
    for (path, prior) in backup {
        match prior {
            Some(servers) => {
                let refs: Vec<&str> = servers.iter().map(|s| s.as_str()).collect();
                set_servers(&store, path, &refs);
            }
            None => {
                let _ = store.remove(CFString::new(path));
            }
        }
    }
}

/// Remove sentinel overrides left by a crashed prior instance.
pub fn cleanup_stale(v4: &str, v6: &str) {
    let store = store();
    for path in all_dns_paths(&store) {
        if let Some(servers) = server_addresses(&store, &path)
            && servers.iter().any(|s| s == v4 || s == v6)
        {
            let _ = store.remove(CFString::new(&path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::state_to_setup_path;

    #[test]
    fn setup_path_derivation() {
        assert_eq!(
            state_to_setup_path("State:/Network/Service/ABC-123/DNS").as_deref(),
            Some("Setup:/Network/Service/ABC-123/DNS")
        );
        assert_eq!(state_to_setup_path("Setup:/Network/Service/X/DNS"), None);
    }
}
