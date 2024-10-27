use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use anyhow::Context;
use bytes::Bytes;

use simple_dns::Packet;
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
    net::UdpSocket,
};

/// A udp-socket-efficient DNS responder.
pub async fn raw_dns_respond(req: Bytes) -> anyhow::Result<Bytes> {
    let (send_resp, recv_resp) = oneshot::channel();
    DNS_RESPONDER
        .send((req, send_resp))
        .await
        .ok()
        .context("could not send")?;
    Ok(recv_resp.await?)
}

static DNS_RESPONDER: LazyLock<Sender<(Bytes, oneshot::Sender<Bytes>)>> = LazyLock::new(|| {
    let (send_req, recv_req) = smol::channel::bounded(1);
    for _ in 0..100 {
        smolscale::spawn(dns_respond_loop(recv_req.clone())).detach();
    }
    send_req
});

async fn dns_respond_loop(recv_req: Receiver<(Bytes, oneshot::Sender<Bytes>)>) {
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    socket.connect("8.8.8.8:53").await.unwrap();
    let outstanding_reqs = Mutex::new(HashMap::new());
    let upload = async {
        loop {
            let (req, send_resp) = recv_req.recv().await.unwrap();
            if let Ok(packet) = Packet::parse(&req) {
                outstanding_reqs
                    .lock()
                    .unwrap()
                    .insert(packet.id(), send_resp);
                let _ = socket.send(&req).await;
            }
        }
    };
    let download = async {
        let mut buf = [0u8; 65536];
        loop {
            let n = socket.recv(&mut buf).await.unwrap();
            if let Ok(packet) = Packet::parse(&buf[..n]) {
                if let Some(resp) = outstanding_reqs.lock().unwrap().remove(&packet.id()) {
                    // tracing::debug!(
                    //     id = packet.id(),
                    //     bytes = hex::encode(&buf[..n]),
                    //     packet = debug(packet),
                    //     "DNS response with CORRECT ID"
                    // );
                    let _ = resp.send(Bytes::copy_from_slice(&buf[..n]));
                } else {
                    tracing::warn!(id = packet.id(), "DNS response with mismatching ID")
                }
            }
        }
    };
    upload.race(download).await
}
