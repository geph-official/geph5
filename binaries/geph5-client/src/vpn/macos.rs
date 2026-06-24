use crate::Config;
use anyctx::AnyCtx;
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use ipstack_geph::IpStack;
use std::net::IpAddr;

pub(super) async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    todo!()
}

pub(super) fn vpn_whitelist(addr: IpAddr) {}
