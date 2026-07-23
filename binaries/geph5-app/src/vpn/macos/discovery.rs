//! Physical-egress discovery via SCDynamicStore, in the style of Mullvad's
//! `talpid-routing` interface monitor.
//!
//! configd normally names the physical egress in `State:/Network/Global/IPv4`,
//! but a NetworkExtension VPN (or stale tunnel state) can legitimately become
//! the global primary service. In that case the underlying active physical
//! service still has its own `State:/Network/Service/<id>/IPv4` record. We
//! enumerate those records and use macOS's configured service order rather than
//! treating the first tunnel as proof that no physical egress exists.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Context, anyhow, bail};
use system_configuration::{
    core_foundation::{array::CFArray, base::TCFType, dictionary::CFDictionary, string::CFString},
    dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder},
};

const GLOBAL_V4: &str = "State:/Network/Global/IPv4";
const GLOBAL_V6: &str = "State:/Network/Global/IPv6";
const SETUP_GLOBAL_V4: &str = "Setup:/Network/Global/IPv4";
const STATE_V4_PATTERN: &str = "State:/Network/Service/.*/IPv4";
const STATE_V4_PREFIX: &str = "State:/Network/Service/";
const STATE_V4_SUFFIX: &str = "/IPv4";

#[derive(Clone, Debug, PartialEq, Eq)]
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

fn dict_string(dict: &CFDictionary, key: &str) -> Option<String> {
    let value = dict.find(CFString::new(key).as_CFType().as_CFTypeRef() as *const _)?;
    let value = unsafe { CFString::wrap_under_get_rule(*value as *const _) };
    Some(value.to_string())
}

fn dict_strings(dict: &CFDictionary, key: &str) -> Option<Vec<String>> {
    let value = dict.find(CFString::new(key).as_CFType().as_CFTypeRef() as *const _)?;
    let value = unsafe { CFArray::<CFString>::wrap_under_get_rule(*value as *const _) };
    Some(value.iter().map(|s| s.to_string()).collect())
}

/// Read `PrimaryInterface`, `Router`, and `PrimaryService` from a
/// `State:/Network/Global/IPv*` dict.
fn global_state(store: &SCDynamicStore, key: &str) -> Option<(String, String, String)> {
    let dict = store
        .get(CFString::new(key))?
        .downcast_into::<CFDictionary>()?;
    Some((
        dict_string(&dict, "PrimaryInterface")?,
        dict_string(&dict, "Router")?,
        dict_string(&dict, "PrimaryService")?,
    ))
}

fn parse_v4(ifname: String, router: String, service_id: String) -> anyhow::Result<PrimaryV4> {
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

fn global_v4(store: &SCDynamicStore) -> anyhow::Result<PrimaryV4> {
    let (ifname, router, service_id) = global_state(store, GLOBAL_V4)
        .context("no primary IPv4 service — not connected to a network?")?;
    parse_v4(ifname, router, service_id)
}

fn service_id_from_v4_path(path: &str) -> Option<&str> {
    path.strip_prefix(STATE_V4_PREFIX)?
        .strip_suffix(STATE_V4_SUFFIX)
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

fn service_v4(store: &SCDynamicStore, path: &str) -> Option<PrimaryV4> {
    let service_id = service_id_from_v4_path(path)?.to_string();
    let dict = store
        .get(CFString::new(path))?
        .downcast_into::<CFDictionary>()?;
    let ifname = dict_string(&dict, "InterfaceName")
        .or_else(|| dict_string(&dict, "ConfirmedInterfaceName"))?;
    let router = dict_string(&dict, "Router")?;
    parse_v4(ifname, router, service_id).ok()
}

fn service_order(store: &SCDynamicStore) -> Vec<String> {
    store
        .get(CFString::new(SETUP_GLOBAL_V4))
        .and_then(|value| value.downcast_into::<CFDictionary>())
        .and_then(|dict| dict_strings(&dict, "ServiceOrder"))
        .unwrap_or_default()
}

fn choose_service_v4(
    mut candidates: Vec<PrimaryV4>,
    configured_order: &[String],
) -> Option<PrimaryV4> {
    candidates.sort_by(|a, b| {
        let rank = |service_id: &str| {
            configured_order
                .iter()
                .position(|id| id == service_id)
                .unwrap_or(usize::MAX)
        };
        rank(&a.service_id)
            .cmp(&rank(&b.service_id))
            .then_with(|| a.service_id.cmp(&b.service_id))
    });
    candidates.into_iter().next()
}

fn physical_service_v4(store: &SCDynamicStore) -> Option<PrimaryV4> {
    let candidates = store
        .get_keys(STATE_V4_PATTERN)
        .map(|keys| {
            keys.iter()
                .filter_map(|path| service_v4(store, &path.to_string()))
                .collect()
        })
        .unwrap_or_default();
    choose_service_v4(candidates, &service_order(store))
}

pub fn primary_v4() -> anyhow::Result<PrimaryV4> {
    let store = store();
    match global_v4(&store) {
        Ok(primary) => Ok(primary),
        Err(global_error) => match physical_service_v4(&store) {
            Some(primary) => {
                tracing::debug!(
                    error = %global_error,
                    interface = %primary.ifname,
                    router = %primary.router,
                    service_id = %primary.service_id,
                    "using active physical IPv4 service behind tunnel primary"
                );
                Ok(primary)
            }
            None => Err(anyhow!(
                "{global_error:#}; no active physical IPv4 service with an IP router was found"
            )),
        },
    }
}

pub fn primary_v6() -> anyhow::Result<PrimaryV6> {
    let store = store();
    let (ifname, router, _service_id) =
        global_state(&store, GLOBAL_V6).context("no primary IPv6 service")?;
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

    fn candidate(service_id: &str, ifname: &str, router: [u8; 4]) -> PrimaryV4 {
        PrimaryV4 {
            ifname: ifname.to_string(),
            router: Ipv4Addr::from(router),
            service_id: service_id.to_string(),
        }
    }

    #[test]
    fn parses_only_v4_service_paths() {
        assert_eq!(
            service_id_from_v4_path("State:/Network/Service/WIFI-ID/IPv4"),
            Some("WIFI-ID")
        );
        assert_eq!(
            service_id_from_v4_path("State:/Network/Service/WIFI-ID/IPv6"),
            None
        );
        assert_eq!(service_id_from_v4_path("State:/Network/Global/IPv4"), None);
    }

    #[test]
    fn physical_service_order_ignores_stale_tunnels() {
        // Tunnel records never make it into this candidate set: service_v4
        // rejects them before selection. The remaining active physical services
        // follow macOS's configured priority, not SCDynamicStore key order.
        let candidates = vec![
            candidate("WIFI", "en1", [192, 168, 1, 1]),
            candidate("ETHERNET", "en0", [10, 0, 0, 1]),
        ];
        let order = vec![
            "STALE-UTUN-1".to_string(),
            "ETHERNET".to_string(),
            "STALE-UTUN-2".to_string(),
            "WIFI".to_string(),
        ];
        let selected = choose_service_v4(candidates, &order).unwrap();
        assert_eq!(selected.service_id, "ETHERNET");
        assert_eq!(selected.ifname, "en0");
    }

    #[test]
    fn physical_service_selection_is_deterministic_without_setup_order() {
        let candidates = vec![
            candidate("SERVICE-B", "en1", [192, 168, 1, 1]),
            candidate("SERVICE-A", "en0", [10, 0, 0, 1]),
        ];
        let selected = choose_service_v4(candidates, &[]).unwrap();
        assert_eq!(selected.service_id, "SERVICE-A");
    }

    #[test]
    fn parse_v4_rejects_tunnels_and_non_ip_routers() {
        assert!(parse_v4("utun6".into(), "10.0.0.1".into(), "VPN".into()).is_err());
        assert!(parse_v4("ipsec0".into(), "10.0.0.1".into(), "VPN".into()).is_err());
        assert!(parse_v4("en1".into(), "link#17".into(), "WIFI".into()).is_err());
    }

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

    // Exercise the service-level path even when the global primary is already a
    // physical interface on the dev Mac.
    #[test]
    fn live_physical_service_v4() {
        let store = store();
        let Ok(global) = global_v4(&store) else {
            return;
        };
        let physical =
            physical_service_v4(&store).expect("global physical service missing from State keys");
        assert_eq!(physical, global);
    }
}
