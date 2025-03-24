use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::CStr;
use std::io::Write;

pub use broker::broker_client;
pub use broker::BrokerSource;
use bytes::Bytes;
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
mod litecopy;
pub mod logging;

mod pac;
mod route;
mod socks5;
mod spoof_dns;
mod stats;
mod taskpool;
mod traffcount;
mod updates;
mod vpn;

// C interface

static CLIENT: OnceCell<Client> = OnceCell::new();

#[no_mangle]
pub unsafe extern "C" fn start_client(cfg: *const c_char) -> libc::c_int {
    let cfg_str = unsafe { CStr::from_ptr(cfg) }.to_str().unwrap();
    let cfg: Config = serde_json::from_str(cfg_str).unwrap();

    CLIENT.get_or_init(|| Client::start(cfg));

    0
}

#[no_mangle]
pub unsafe extern "C" fn daemon_rpc(
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

#[no_mangle]
pub unsafe extern "C" fn send_pkt(pkt: *const c_char, pkt_len: c_int) -> c_int {
    let slice: &'static [u8] = std::slice::from_raw_parts(pkt as *mut u8, pkt_len as usize);
    if let Some(client) = CLIENT.get() {
        if let Ok(_) = smol::future::block_on(client.send_vpn_packet(Bytes::copy_from_slice(slice)))
        {
            return 0;
        }
    }
    -1
}

#[no_mangle]
pub unsafe extern "C" fn recv_pkt(out_buf: *mut c_char, out_buflen: c_int) -> c_int {
    if let Some(client) = CLIENT.get() {
        if let Ok(pkt) = smol::future::block_on(client.recv_vpn_packet()) {
            return fill_buffer(out_buf, out_buflen, &pkt);
        }
    }
    -1
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

#[cfg(test)]
mod tests {
    use smol::Timer;

    use super::*;
    use std::{
        ffi::CString,
        net::{Ipv4Addr, SocketAddr},
    };

    const CONTROL_ADDR: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12222);

    pub const PAC_ADDR: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12223);

    const SOCKS5_ADDR: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9909);

    pub const HTTP_ADDR: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9910);

    #[test]
    fn test_clib() {
        let cfg = super::Config {
            // These fields are the base defaults:
            socks5_listen: Some(SOCKS5_ADDR),
            http_proxy_listen: Some(HTTP_ADDR),
            control_listen: Some(CONTROL_ADDR),
            exit_constraint: super::ExitConstraint::Auto,
            bridge_mode: BridgeMode::Auto,
            cache: None,
            broker: Some(BrokerSource::Race(vec![
                BrokerSource::Fronted {
                    front: "https://www.cdn77.com/".into(),
                    host: "1826209743.rsc.cdn77.org".into(),
                },
                BrokerSource::Fronted {
                    front: "https://vuejs.org/".into(),
                    host: "svitania-naidallszei-2.netlify.app".into(),
                },
                BrokerSource::AwsLambda {
                    function_name: "geph-lambda-bouncer".into(),
                    region: "us-east-1".into(),
                    access_key_id: String::from_utf8_lossy(
                        &base32::decode(
                            base32::Alphabet::Crockford,
                            "855MJGAMB58MCPJBB97K4P2C6NC36DT8",
                        )
                        .unwrap(),
                    )
                    .to_string(),
                    secret_access_key: String::from_utf8_lossy(
                        &base32::decode(
                            base32::Alphabet::Crockford,
                            "8SQ7ECABES132WT4B9GQEN356XQ6GRT36NS64GBK9HP42EAGD8W6JRA39DTKAP2J",
                        )
                        .unwrap(),
                    )
                    .to_string(),
                },
            ])),
            broker_keys: Some(BrokerKeys {
                master: "88c1d2d4197bed815b01a22cadfc6c35aa246dddb553682037a118aebfaa3954".into(),
                mizaru_free: "0558216cbab7a9c46f298f4c26e171add9af87d0694988b8a8fe52ee932aa754"
                    .into(),
                mizaru_plus: "cf6f58868c6d9459b3a63bc2bd86165631b3e916bad7f62b578cd9614e0bcb3b"
                    .into(),
            }),
            // Values that can be overridden by `args`:
            vpn: false,
            spoof_dns: false,
            passthrough_china: false,
            dry_run: false,
            credentials: geph5_broker_protocol::Credential::Secret(String::new()),
            sess_metadata: Default::default(),
            task_limit: None,
            pac_listen: Some(PAC_ADDR),
        };
        let cfg_str = CString::new(serde_json::to_string(&cfg).unwrap()).unwrap();
        let cfg_ptr = cfg_str.as_ptr();

        let start_client_ret = unsafe { start_client(cfg_ptr) };
        assert!(start_client_ret == 0);

        // call daemon_rpc;
        for _ in 0..2 {
            let jrpc_req = JrpcRequest {
                jsonrpc: "2.0".into(),
                method: "user_info".into(),
                params: [].into(),
                id: nanorpc::JrpcId::Number(1),
            };
            let jrpc_req_str = CString::new(serde_json::to_string(&jrpc_req).unwrap()).unwrap();
            let jrpc_req_ptr = jrpc_req_str.as_ptr();
            // Allocate a buffer for the response
            let mut out_buf = vec![0; 1024 * 128]; // Adjust size as needed
            let out_buf_ptr = out_buf.as_mut_ptr();

            let rpc_ret = unsafe { daemon_rpc(jrpc_req_ptr, out_buf_ptr, out_buf.len() as _) };
            println!("daemon_rpc retcode = {rpc_ret}");
            assert!(rpc_ret >= 0);
            let output = unsafe { CStr::from_ptr(out_buf_ptr) }.to_str().unwrap();
            println!("daemon_rpc output = {output}");
            smolscale::block_on(async { Timer::after(std::time::Duration::from_secs(1)).await });
        }
    }
}
