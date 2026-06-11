use std::{net::SocketAddr, sync::LazyLock, time::Duration};

use geph5_broker_protocol::{BrokerClient, Mac};
use geph5_stats::StatBatcher;
use sillad::{dialer::DialerExt, tcp::TcpDialer};
use smol_timeout2::TimeoutExt;

/// Stats accumulated locally, shipped to the broker in periodic batches.
pub static STAT_BATCHER: LazyLock<StatBatcher> = LazyLock::new(StatBatcher::new);

const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Periodically drains STAT_BATCHER into the broker's authenticated report_stats RPC.
pub async fn stats_flush_loop(auth_token: &str, broker_addr: SocketAddr) {
    let broker_rpc = BrokerClient(nanorpc_sillad::DialerTransport(
        TcpDialer {
            dest_addr: broker_addr,
        }
        .timeout(Duration::from_secs(1)),
    ));
    let mac_key = blake3::hash(auth_token.as_bytes());

    loop {
        async_io::Timer::after(FLUSH_INTERVAL).await;
        let events = STAT_BATCHER.drain();
        if events.is_empty() {
            continue;
        }
        let res = broker_rpc
            .report_stats(Mac::new(events, mac_key.as_bytes()))
            .timeout(Duration::from_secs(5))
            .await;
        match res {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(err))) => tracing::warn!(err = %err, "broker rejected stats batch"),
            Some(Err(err)) => tracing::warn!(err = %err, "failed to ship stats batch"),
            None => tracing::warn!("shipping stats batch timed out"),
        }
    }
}
