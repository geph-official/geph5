//! Publish the tun's IPv6 onto the primary service's
//! `State:/Network/Service/<id>/IPv6` while the VPN is up.
//!
//! macOS derives each resolver's "Request A/AAAA records" flags from the owning
//! service's address families: a v4-only primary service means mDNSResponder
//! never requests AAAA and `getaddrinfo(AF_INET6)` returns nothing, even with
//! working v6 routes and datapath. Claiming IPv6 on the primary service (with
//! the tun as its interface, the shape a NetworkExtension VPN would present)
//! turns AAAA resolution on. `State:` only — volatile, owned by configd, which
//! re-derives it from the real interface state after our key is removed.

use system_configuration::{
    core_foundation::{
        array::CFArray,
        base::{CFType, TCFType},
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    },
    dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder},
};

const STATE_V6_PATTERN: &str = "State:/Network/Service/.*/IPv6";

fn store() -> SCDynamicStore {
    SCDynamicStoreBuilder::new("io.geph.manager.ipv6").build()
}

fn v6_dict(tun_ifname: &str, tun_v6: &str) -> CFDictionary<CFString, CFType> {
    let addrs = CFArray::from_CFTypes(&[CFString::new(tun_v6)]);
    let prefixes = CFArray::from_CFTypes(&[CFNumber::from(64i32)]);
    CFDictionary::from_CFType_pairs(&[
        (CFString::new("Addresses"), addrs.as_CFType()),
        (CFString::new("PrefixLength"), prefixes.as_CFType()),
        (CFString::new("InterfaceName"), CFString::new(tun_ifname).as_CFType()),
        (CFString::new("Router"), CFString::new(tun_v6).as_CFType()),
    ])
}

/// Claim IPv6 on `service_id`, returning the written path for [`remove`].
pub fn publish(service_id: &str, tun_ifname: &str, tun_v6: &str) -> Option<String> {
    let path = format!("State:/Network/Service/{service_id}/IPv6");
    store()
        .set(CFString::new(&path), v6_dict(tun_ifname, tun_v6).to_untyped())
        .then_some(path)
}

/// Drop our claim; configd re-derives the service's real IPv6 state from the
/// interface on its next evaluation.
pub fn remove(path: &str) {
    let _ = store().remove(CFString::new(path));
}

/// Remove claims left by a crashed prior instance, identified by the tun
/// address (`marker`) in the dict's `Addresses`.
pub fn cleanup_stale(marker: &str) {
    let store = store();
    let paths: Vec<String> = store
        .get_keys(STATE_V6_PATTERN)
        .map(|keys| keys.iter().map(|k| k.to_string()).collect())
        .unwrap_or_default();
    for path in paths {
        let has_marker = (|| {
            let dict = store
                .get(CFString::new(&path))?
                .downcast_into::<CFDictionary>()?;
            let v =
                dict.find(CFString::new("Addresses").as_CFType().as_CFTypeRef() as *const _)?;
            let arr = unsafe { CFArray::<CFString>::wrap_under_get_rule(*v as *const _) };
            Some(arr.iter().any(|s| s.to_string().starts_with(marker)))
        })()
        .unwrap_or(false);
        if has_marker {
            let _ = store.remove(CFString::new(&path));
        }
    }
}
