use crate::database::bridges::BridgeMetadata;
use geph5_broker_protocol::AccountLevel;

pub(crate) fn filter_raw_bridge_descriptors(
    raw_descriptors: impl IntoIterator<Item = BridgeMetadata>,
    account_level: AccountLevel,
    country: &str,
) -> Vec<BridgeMetadata> {
    raw_descriptors
        .into_iter()
        .filter(|meta| account_level == AccountLevel::Plus || !meta.is_plus)
        .filter(|meta| {
            let pool = meta.descriptor.pool.as_str();
            if country == "CN" && meta.china_fail_count > meta.china_success_count {
                tracing::trace!(
                    "filtering out {}/{} due to GFW blocking in China",
                    meta.descriptor.pool,
                    meta.descriptor.control_listen.ip()
                );
                return false;
            }
            for only in ["CN", "TM", "IR"] {
                let no_tag = format!("NO{only}");
                if pool.contains(no_tag.as_str()) {
                    if country == only {
                        return false;
                    }
                    continue;
                }
                if pool.contains(only) {
                    return country == only;
                }
            }
            true
        })
        .map(|mut meta| {
            if account_level == AccountLevel::Plus && !meta.is_plus {
                meta.delay_ms = meta.delay_ms.saturating_add(500);
            }
            meta
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge(pool: &str, is_plus: bool) -> BridgeMetadata {
        BridgeMetadata {
            descriptor: geph5_broker_protocol::BridgeDescriptor {
                control_listen: "192.0.2.1:12345".parse().unwrap(),
                control_cookie: "test".into(),
                pool: pool.into(),
                expiry: u64::MAX,
            },
            delay_ms: 0,
            is_plus,
            china_success_count: 0,
            china_fail_count: 0,
        }
    }

    #[test]
    fn china_routes_exclude_bridges_with_more_failures_than_successes() {
        for (tier, plus) in [(AccountLevel::Free, false), (AccountLevel::Plus, true)] {
            for (successes, failures, allowed) in [
                (0, 0, true),
                (0, 1, false),
                (2, 3, false),
                (3, 3, true),
                (4, 3, true),
            ] {
                let mut meta = bridge("test_CN", plus);
                meta.china_success_count = successes;
                meta.china_fail_count = failures;
                let routes = filter_raw_bridge_descriptors([meta], tier, "CN");
                assert_eq!(!routes.is_empty(), allowed, "{successes} / {failures}");
            }
        }
    }

    #[test]
    fn china_probe_failures_do_not_remove_routes_outside_china() {
        for country in ["US", "IR", "TM"] {
            let mut meta = bridge("test", false);
            meta.china_fail_count = 100;
            let routes = filter_raw_bridge_descriptors([meta], AccountLevel::Free, country);
            assert_eq!(routes.len(), 1);
        }
    }

    #[test]
    fn plus_gets_all_routes_with_an_extra_delay_for_free_routes() {
        for tier in [AccountLevel::Free, AccountLevel::Plus] {
            let mut free = bridge("free", false);
            free.delay_ms = 75;
            let mut plus = bridge("plus", true);
            plus.delay_ms = 25;
            let routes = filter_raw_bridge_descriptors([free, plus], tier, "CN");
            if tier == AccountLevel::Plus {
                assert_eq!(routes.len(), 2);
                assert_eq!(routes[0].delay_ms, 575);
                assert_eq!(routes[1].delay_ms, 25);
            } else {
                assert_eq!(routes.len(), 1);
                assert!(!routes[0].is_plus);
                assert_eq!(routes[0].delay_ms, 75);
            }
        }
    }

    #[test]
    fn country_restrictions_still_apply() {
        for (pool, country, allowed) in [
            ("test_NOCN", "CN", false),
            ("test_CN", "US", false),
            ("test_IR", "CN", false),
            ("test_NOIR", "CN", true),
            ("test_CN", "CN", true),
        ] {
            let routes =
                filter_raw_bridge_descriptors([bridge(pool, false)], AccountLevel::Free, country);
            assert_eq!(!routes.is_empty(), allowed, "{pool} / {country}");
        }
    }
}
