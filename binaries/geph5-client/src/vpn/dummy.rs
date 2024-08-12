use anyctx::AnyCtx;
use bytes::Bytes;
use ipstack_geph::IpStack;
use smol::channel::{Receiver, Sender};
use std::net::IpAddr;

use crate::Config;

pub(super) async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
}

pub fn vpn_whitelist(_addr: IpAddr) {
    // noop
}
