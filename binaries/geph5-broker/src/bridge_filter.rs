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
            if country == "CN" && meta.china_fail_count > meta.china_success_count {
                tracing::trace!(
                    "filtering out {}/{} due to GFW blocking in China",
                    meta.descriptor.pool,
                    meta.descriptor.control_listen.ip()
                );
                return false;
            }
            for only in ["CN", "TM", "IR"] {
                if meta.descriptor.pool.contains(only) {
                    return country == only;
                }
            }
            true
        })
        .collect()
}
