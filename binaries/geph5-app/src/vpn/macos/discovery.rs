//! Physical-egress discovery via SCDynamicStore, in the style of Mullvad's
//! `talpid-routing` interface monitor: configd's `State:/Network/Global/IPv4`
//! and `.../IPv6` name the primary service's interface and router directly, and
//! our own utun can never appear there.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Context, bail};
use system_configuration::{
    core_foundation::{base::TCFType, dictionary::CFDictionary, string::CFString},
    dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder},
};

pub struct PrimaryV4 {
    pub ifname: String,
    pub router: Ipv4Addr,
    /// configd service ID of the primary service (e.g. the Wi-Fi service UUID).
    pub service_id: String,
}

pub struct PrimaryV6 {
    pub ifname: String,
    pub router: Ipv6Addr,
}

fn store() -> SCDynamicStore {
    SCDynamicStoreBuilder::new("io.geph.manager.vpn").build()
}

/// Read `PrimaryInterface`, `Router`, and `PrimaryService` from a
/// `State:/Network/Global/IPv*` dict.
fn global_state(store: &SCDynamicStore, key: &str) -> Option<(String, String, String)> {
    let dict = store
        .get(CFString::new(key))?
        .downcast_into::<CFDictionary>()?;
    let get = |k: &str| -> Option<String> {
        let v = dict.find(CFString::new(k).as_CFType().as_CFTypeRef() as *const _)?;
        let s = unsafe { CFString::wrap_under_get_rule(*v as *const _) };
        Some(s.to_string())
    };
    Some((
        get("PrimaryInterface")?,
        get("Router")?,
        get("PrimaryService")?,
    ))
}

pub fn primary_v4() -> anyhow::Result<PrimaryV4> {
    let store = store();
    let (ifname, router, service_id) = global_state(&store, "State:/Network/Global/IPv4")
        .context("no primary IPv4 service — not connected to a network?")?;
    if super::is_tunnel_iface(&ifname) {
        bail!("primary IPv4 interface is a tunnel ({ifname})");
    }
    let router: Ipv4Addr = router
        .parse()
        .with_context(|| format!("primary IPv4 router {router:?} is not an IPv4 address"))?;
    Ok(PrimaryV4 {
        ifname,
        router,
        service_id,
    })
}

pub fn primary_v6() -> anyhow::Result<PrimaryV6> {
    let store = store();
    let (ifname, router, _service_id) = global_state(&store, "State:/Network/Global/IPv6")
        .context("no primary IPv6 service")?;
    if super::is_tunnel_iface(&ifname) {
        bail!("primary IPv6 interface is a tunnel ({ifname})");
    }
    // Link-local routers carry a %zone suffix; the address part is what routes.
    let addr = router.split('%').next().unwrap_or(&router);
    let router: Ipv6Addr = addr
        .parse()
        .with_context(|| format!("primary IPv6 router {router:?} is not an IPv6 address"))?;
    Ok(PrimaryV6 { ifname, router })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live smoke test on the dev Mac: whatever configd reports as primary must
    // be a real NIC with a parseable router, or an error — never a tunnel.
    #[test]
    fn live_primary_v4() {
        match primary_v4() {
            Ok(p) => {
                assert!(!super::super::is_tunnel_iface(&p.ifname));
                assert!(!p.ifname.is_empty());
            }
            Err(e) => eprintln!("no primary v4 ({e}); nothing to assert"),
        }
    }
}
