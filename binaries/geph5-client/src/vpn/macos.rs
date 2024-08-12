use crate::Config;
use anyctx::AnyCtx;
use bytes::Bytes;
use ipstack_geph::IpStack;
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
};
use std::net::IpAddr;

pub(super) async fn packet_shuffle(
    ctx: AnyCtx<Config>,
    send_captured: Sender<Bytes>,
    recv_injected: Receiver<Bytes>,
) -> anyhow::Result<()> {
    todo!()
}

pub fn vpn_whitelist(addr: IpAddr) {}
