use std::net::IpAddr;

use anyctx::AnyCtx;
use ipstack_geph::IpStack;

use crate::Config;

pub(super) async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    todo!()
}
