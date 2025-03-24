use std::{env, sync::LazyLock};
use nano_influxdb::InfluxDbEndpoint;

/// Global LazyLock for InfluxDB endpoint configuration, reads from environment variables:
pub static INFLUXDB_ENDPOINT: LazyLock<Option<InfluxDbEndpoint>> = LazyLock::new(|| {
    let url = env::var("GEPH5_BRIDGE_INFLUXDB").ok()?;
    let username = env::var("GEPH5_BRIDGE_INFLUXDB_USERNAME").ok()?;
    let password = env::var("GEPH5_BRIDGE_INFLUXDB_PASSWORD").ok()?;

    Some(InfluxDbEndpoint {
        url,
        username,
        password,
    })
});