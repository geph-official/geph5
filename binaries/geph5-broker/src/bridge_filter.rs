use crate::database::bridges::BridgeMetadata;
use geph5_broker_protocol::AccountLevel;

pub(crate) fn filter_raw_bridge_descriptors(
    raw_descriptors: impl IntoIterator<Item = BridgeMetadata>,
    account_level: AccountLevel,
    country: &str,
) -> Vec<BridgeMetadata> {
    raw_descriptors
        .into_iter()
        .filter(|meta| account_level != AccountLevel::Free || !meta.is_plus)
        .filter(|meta| {
            let pool = meta.descriptor.pool.as_str();

            // For China Plus users, filter out ovh.
            if country == "CN" && meta.china_fail_count > meta.china_success_count {
                tracing::trace!(
                    "filtering out {}/{} due to GFW blocking in China",
                    meta.descriptor.pool,
                    meta.descriptor.control_listen.ip()
                );
                return false;
            }
            if account_level == AccountLevel::Plus
                && country == "CN"
                && meta.descriptor.pool.contains("ovh")
            {
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
        .collect()
}
