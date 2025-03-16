use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::CStr;
use std::io::Write;

pub use broker::broker_client;
pub use broker::BrokerSource;
pub use client::Client;
pub use client::{BridgeMode, BrokerKeys, Config};
pub use control_prot::{ConnInfo, ControlClient};
use nanorpc::JrpcRequest;
use nanorpc::RpcTransport;
use once_cell::sync::OnceCell;
pub use route::ExitConstraint;

mod auth;
mod broker;
mod china;
mod client;
mod client_inner;
mod control_prot;
mod database;
mod http_proxy;
pub mod logging;

mod pac;
mod route;
mod socks5;
mod spoof_dns;
mod stats;
mod taskpool;
mod vpn;

// C interface

static CLIENT: OnceCell<Client> = OnceCell::new();

#[no_mangle]
pub extern "C" fn start_client(cfg: *const c_char) -> libc::c_int {
    let cfg_str = unsafe { CStr::from_ptr(cfg) }.to_str().unwrap();
    let cfg: Config = serde_json::from_str(cfg_str).unwrap();

    CLIENT.get_or_init(|| Client::start(cfg));

    0
}

#[no_mangle]
pub extern "C" fn daemon_rpc(
    jrpc_req: *const c_char,
    out_buf: *mut c_char,
    out_buflen: c_int,
) -> c_int {
    let req_str = unsafe { CStr::from_ptr(jrpc_req) }.to_str().unwrap();
    let jrpc: JrpcRequest = serde_json::from_str(req_str).unwrap();

    if let Some(client) = CLIENT.get() {
        let ctrl = client.control_client().0;
        if let Ok(response) = smolscale::block_on(async move { ctrl.call_raw(jrpc).await }) {
            let response_json = serde_json::to_string(&response).unwrap();
            let response_c = std::ffi::CString::new(response_json).unwrap();
            let bytes = response_c.as_bytes_with_nul();

            unsafe { fill_buffer(out_buf, out_buflen, bytes) }
        } else {
            -2 // jrpc error
        }
    } else {
        -1 // daemon not started
    }
}

unsafe fn fill_buffer(buffer: *mut c_char, buflen: c_int, output: &[u8]) -> c_int {
    let mut slice = std::slice::from_raw_parts_mut(buffer as *mut u8, buflen as usize);
    if output.len() < slice.len() {
        if slice.write_all(output).is_err() {
            tracing::debug!("writing to buffer failed!");
            -4
        } else {
            output.len() as c_int
        }
    } else {
        tracing::debug!(" buffer not big enough!");
        -3
    }
}
