use std::net::IpAddr;

use anyctx::AnyCtx;
use ipstack_geph::IpStack;

use crate::Config;

pub struct VpnCapture {
    ipstack: IpStack,
}

impl VpnCapture {
    pub fn new(ctx: AnyCtx<Config>) -> Self {
        todo!()
    }

    pub fn ipstack(&self) -> &IpStack {
        &self.ipstack
    }
}

pub fn vpn_whitelist(addr: IpAddr) {
    // noop
}
