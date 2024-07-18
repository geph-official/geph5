use std::net::IpAddr;

use anyctx::AnyCtx;
use ipstack_geph::IpStack;

use crate::Config;

pub struct VpnCapture {
    ipstack: IpStack,
}

impl VpnCapture {
    pub fn new(_ctx: AnyCtx<Config>) -> Self {
        todo!()
    }

    pub fn ipstack(&self) -> &IpStack {
        &self.ipstack
    }
}

pub fn vpn_whitelist(_addr: IpAddr) {
    // noop
}
