pub use broker::broker_client;
pub use broker::BrokerSource;
pub use client::Client;
pub use client::{BridgeMode, BrokerKeys, Config};
pub use control_prot::{ConnInfo, ControlClient};
use nanorpc::JrpcId;
use nanorpc::JrpcRequest;
use nanorpc::RpcTransport;
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

// C library for iOS

use std::sync::Once;

use serde_json::Value;
static mut CLIENT: Option<Client> = None;
static INIT: Once = Once::new();

#[no_mangle]
pub extern "C" fn start_geph5_client(cfg: *const libc::c_char) -> libc::c_int {
    // Convert C string to Rust string, return -1 on failure
    let cfg_str = match unsafe { std::ffi::CStr::from_ptr(cfg) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };

    // Parse config, return -1 on failure
    let cfg: Config = match serde_json::from_str(cfg_str) {
        Ok(c) => c,
        Err(_) => return -2,
    };

    unsafe {
        INIT.call_once(|| {
            CLIENT = Some(Client::start(cfg));
        });
    }
    0
}

#[no_mangle]
pub extern "C" fn daemon_rpc(
    method: *const libc::c_char,
    params: *const libc::c_char,
) -> *mut libc::c_char {
    fn handle_rpc_call(
        method: *const libc::c_char,
        params: *const libc::c_char,
    ) -> anyhow::Result<String> {
        let method = unsafe { std::ffi::CStr::from_ptr(method) }
            .to_str()?
            .to_owned();
        let params_str = unsafe { std::ffi::CStr::from_ptr(params) }.to_str()?;
        let params: Vec<Value> = serde_json::from_str(params_str)?;
        let jrpc = JrpcRequest {
            jsonrpc: "2.0".into(),
            method,
            params,
            id: JrpcId::Number(1),
        };

        let client =
            unsafe { CLIENT.as_ref() }.ok_or_else(|| anyhow::anyhow!("no client running"))?;
        let ctrl = client.control_client();
        let resp = smol::block_on(ctrl.0.call_raw(jrpc))?;
        Ok(serde_json::to_string(&resp)?)
    }

    let result = match handle_rpc_call(method, params) {
        Ok(ok) => ok,
        Err(e) => serde_json::to_string(&nanorpc::JrpcError {
            code: -1,
            message: e.to_string(),
            data: serde_json::Value::Null,
        })
        .unwrap(),
    };

    std::ffi::CString::new(result)
        .unwrap_or_else(|_| std::ffi::CString::new("null").unwrap())
        .into_raw()
}
