//! A shared, connected UDP socket for each upstream DNS server.
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use moka::future::Cache;
use simple_dns::{OPCODE, Packet, PacketFlag, Question};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::oneshot,
};

pub(super) async fn respond(server: SocketAddr, request: Bytes) -> anyhow::Result<Bytes> {
    static CLIENTS: LazyLock<Cache<SocketAddr, Arc<UdpDnsClient>>> =
        LazyLock::new(|| Cache::new(16));
    let client = CLIENTS
        .try_get_with(server, async {
            UdpDnsClient::new(server).await.map(Arc::new)
        })
        .await
        .map_err(|e: Arc<anyhow::Error>| anyhow::anyhow!("{e:#}"))?;
    client.request(request).await
}

struct Pending {
    questions: Vec<Question<'static>>,
    opcode: OPCODE,
    sender: Option<oneshot::Sender<Bytes>>,
}

impl Pending {
    fn matches(&self, response: &Packet<'_>) -> bool {
        response.has_flags(PacketFlag::RESPONSE)
            && response.opcode() == self.opcode
            && response.questions.len() == self.questions.len()
            && response
                .questions
                .iter()
                .zip(&self.questions)
                .all(|(a, b)| {
                    a.qname.iter().len() == b.qname.iter().len()
                        && a.qname
                            .iter()
                            .zip(b.qname.iter())
                            .all(|(a, b)| a.as_ref().eq_ignore_ascii_case(b.as_ref()))
                        && a.qtype == b.qtype
                        && a.qclass == b.qclass
                })
    }
}

type Requests = Arc<Mutex<HashMap<u16, Pending>>>;

struct UdpDnsClient {
    socket: Arc<UdpSocket>,
    requests: Requests,
    // Dropping the client also stops its receiver; the task does not own the client.
    _receiver: geph5_rt::Task<()>,
}

impl UdpDnsClient {
    async fn new(server: SocketAddr) -> anyhow::Result<Self> {
        let bind = if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        // The kernel rejects packets from any other source address/port.
        socket.connect(server).await?;
        tracing::info!(%server, local_addr = %socket.local_addr()?, "created shared UDP DNS socket");
        let requests: Requests = Arc::new(Mutex::new(HashMap::new()));
        let receiver_socket = socket.clone();
        let receiver_requests = requests.clone();
        let receiver = geph5_rt::spawn(async move {
            let mut buf = vec![0; 65535];
            loop {
                let len = match receiver_socket.recv(&mut buf).await {
                    Ok(len) => len,
                    Err(err) => {
                        // A connected UDP socket can report a transient ICMP error.
                        // Keep the receiver alive; outstanding requests still time out.
                        tracing::warn!(%server, %err, "UDP DNS receive failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Ok(response) = Packet::parse(&buf[..len]) else {
                    continue;
                };
                let mut requests = receiver_requests.lock().unwrap();
                if let Some(pending) = requests.get_mut(&response.id())
                    && pending.matches(&response)
                    && let Some(sender) = pending.sender.take()
                {
                    let _ = sender.send(Bytes::copy_from_slice(&buf[..len]));
                }
            }
        });
        Ok(Self {
            socket,
            requests,
            _receiver: receiver,
        })
    }

    async fn request(&self, request: Bytes) -> anyhow::Result<Bytes> {
        let packet = Packet::parse(&request).context("invalid UDP DNS query")?;
        anyhow::ensure!(
            !packet.has_flags(PacketFlag::RESPONSE),
            "expected a DNS query"
        );
        anyhow::ensure!(!packet.questions.is_empty(), "DNS query has no questions");
        let original_id = packet.id();
        let (sender, receiver) = oneshot::channel();
        let id = {
            let mut requests = self.requests.lock().unwrap();
            let start: u16 = rand::random();
            let id = (0..=u16::MAX)
                .map(|offset| start.wrapping_add(offset))
                .find(|id| !requests.contains_key(id))
                .context("UDP DNS transaction IDs exhausted")?;
            requests.insert(
                id,
                Pending {
                    questions: packet
                        .questions
                        .iter()
                        .cloned()
                        .map(Question::into_owned)
                        .collect(),
                    opcode: packet.opcode(),
                    sender: Some(sender),
                },
            );
            id
        };
        // Retain the ID until this request finishes, including timeout/cancellation.
        // The receiver takes only the sender so cleanup cannot remove a newer query.
        let _registration = Registration {
            requests: &self.requests,
            id,
        };
        let mut wire = request.to_vec();
        wire[..2].copy_from_slice(&id.to_be_bytes());
        self.socket
            .send(&wire)
            .await
            .context("sending UDP DNS query")?;
        let response = receiver.await.context("UDP DNS receiver stopped")?;
        let mut response = response.to_vec();
        if Packet::parse(&response)?.has_flags(PacketFlag::TRUNCATION) {
            // Standard DNS TCP fallback, within the caller's original timeout.
            let mut stream = TcpStream::connect(self.socket.peer_addr()?).await?;
            stream.write_u16(wire.len().try_into()?).await?;
            stream.write_all(&wire).await?;
            let len = stream.read_u16().await?;
            response.resize(len as usize, 0);
            stream.read_exact(&mut response).await?;
            let packet = Packet::parse(&response)?;
            let requests = self.requests.lock().unwrap();
            anyhow::ensure!(
                packet.id() == id
                    && requests[&id].matches(&packet)
                    && !packet.has_flags(PacketFlag::TRUNCATION),
                "invalid TCP DNS fallback response"
            );
        }
        response[..2].copy_from_slice(&original_id.to_be_bytes());
        Ok(response.into())
    }
}

struct Registration<'a> {
    requests: &'a Requests,
    id: u16,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.requests.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_dns::{CLASS, Name, QCLASS, QTYPE, ResourceRecord, TYPE, rdata::RData};

    fn query(name: &str) -> Bytes {
        let mut packet = Packet::new_query(123);
        packet.questions.push(Question::new(
            Name::new_unchecked(name),
            QTYPE::TYPE(TYPE::A),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        packet.build_bytes_vec().unwrap().into()
    }

    fn reply(query: &[u8]) -> Vec<u8> {
        let mut packet = Packet::parse(query).unwrap().into_reply();
        packet.answers.push(ResourceRecord::new(
            packet.questions[0].qname.clone(),
            CLASS::IN,
            60,
            RData::A(std::net::Ipv4Addr::new(203, 0, 113, 7).into()),
        ));
        packet.build_bytes_vec().unwrap()
    }

    #[test]
    fn shared_socket_demultiplexes_colliding_ids_and_reordered_replies() {
        geph5_rt::block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let addr = server.local_addr().unwrap();
                let stub = geph5_rt::spawn(async move {
                    let mut buf = [0; 4096];
                    let (n, source1) = server.recv_from(&mut buf).await.unwrap();
                    let first = buf[..n].to_vec();
                    let (n, source2) = server.recv_from(&mut buf).await.unwrap();
                    let second = buf[..n].to_vec();
                    assert_eq!(source1, source2, "queries must share the socket");
                    assert_ne!(&first[..2], &second[..2], "upstream IDs must be unique");

                    // Correct ID, wrong question; then a non-response and malformed packet.
                    let mut wrong = reply(&second);
                    wrong[..2].copy_from_slice(&first[..2]);
                    server.send_to(&wrong, source1).await.unwrap();
                    server.send_to(&first, source1).await.unwrap();
                    server.send_to(&[0], source1).await.unwrap();
                    // Correct-looking packet from the wrong server must also be ignored.
                    let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    stranger.send_to(&wrong, source1).await.unwrap();
                    server.send_to(&reply(&second), source2).await.unwrap();
                    server.send_to(&reply(&first), source1).await.unwrap();

                    // A subsequent request should reuse that socket, too.
                    let (n, source3) = server.recv_from(&mut buf).await.unwrap();
                    assert_eq!(source1, source3);
                    server.send_to(&reply(&buf[..n]), source3).await.unwrap();
                });
                let (first, second) = futures_util::future::join(
                    respond(addr, query("first.example")),
                    respond(addr, query("second.example")),
                )
                .await;
                for (response, name) in [
                    (first.unwrap(), "first.example"),
                    (second.unwrap(), "second.example"),
                ] {
                    let packet = Packet::parse(&response).unwrap();
                    assert_eq!(packet.id(), 123);
                    assert_eq!(packet.questions[0].qname.to_string(), name);
                    assert_eq!(packet.answers.len(), 1);
                }
                respond(addr, query("third.example")).await.unwrap();
                stub.await;
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn timeout_cleans_up_and_socket_remains_usable() {
        geph5_rt::block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let client = UdpDnsClient::new(server.local_addr().unwrap())
                    .await
                    .unwrap();
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(20),
                        client.request(query("lost.example"))
                    )
                    .await
                    .is_err()
                );
                assert!(client.requests.lock().unwrap().is_empty());
                let mut buf = [0; 4096];
                let (n, source1) = server.recv_from(&mut buf).await.unwrap();
                let stale = reply(&buf[..n]);
                let stub = geph5_rt::spawn(async move {
                    let (n, source2) = server.recv_from(&mut buf).await.unwrap();
                    assert_eq!(source1, source2);
                    server.send_to(&stale, source2).await.unwrap();
                    server.send_to(&reply(&buf[..n]), source2).await.unwrap();
                });
                let response = client.request(query("next.example")).await.unwrap();
                assert_eq!(
                    Packet::parse(&response).unwrap().questions[0]
                        .qname
                        .to_string(),
                    "next.example"
                );
                assert!(client.requests.lock().unwrap().is_empty());
                stub.await;
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn truncated_udp_response_uses_tcp_to_same_server() {
        geph5_rt::block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let addr = udp.local_addr().unwrap();
                let tcp = tokio::net::TcpListener::bind(addr).await.unwrap();
                let stub = geph5_rt::spawn(async move {
                    let mut buf = [0; 4096];
                    let (n, source) = udp.recv_from(&mut buf).await.unwrap();
                    let wire = buf[..n].to_vec();
                    let mut truncated = Packet::parse(&wire).unwrap().into_reply();
                    truncated.set_flags(PacketFlag::TRUNCATION);
                    udp.send_to(&truncated.build_bytes_vec().unwrap(), source)
                        .await
                        .unwrap();
                    let (mut stream, _) = tcp.accept().await.unwrap();
                    let n = stream.read_u16().await.unwrap() as usize;
                    stream.read_exact(&mut buf[..n]).await.unwrap();
                    assert_eq!(&wire, &buf[..n]);
                    let response = reply(&wire);
                    stream.write_u16(response.len() as u16).await.unwrap();
                    stream.write_all(&response).await.unwrap();
                });
                let response = respond(addr, query("large.example")).await.unwrap();
                let packet = Packet::parse(&response).unwrap();
                assert_eq!(packet.id(), 123);
                assert!(!packet.has_flags(PacketFlag::TRUNCATION));
                assert_eq!(packet.answers.len(), 1);
                stub.await;
            })
            .await
            .unwrap();
        });
    }
}
