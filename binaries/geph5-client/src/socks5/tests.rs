use super::*;
use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context as TaskContext, Poll},
};
use tokio::{
    io::{DuplexStream, ReadBuf},
    net::TcpStream,
    task::JoinHandle,
};

struct TestPipe(DuplexStream);

impl AsyncRead for TestPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for TestPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl sillad::Pipe for TestPipe {
    fn protocol(&self) -> &str {
        "test"
    }
    fn remote_addr(&self) -> Option<&str> {
        None
    }
}

fn unused_opener() -> TunnelOpener {
    Arc::new(|_, _| Box::pin(async { panic!("request must not open a tunnel") }))
}

async fn start_proxy(
    bind: &str,
    open: TunnelOpener,
) -> (TcpStream, JoinHandle<anyhow::Result<()>>) {
    let mut listener = sillad::tcp::TcpListener::bind(bind.parse().unwrap())
        .await
        .unwrap();
    let mut address = listener.local_addr().await;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            "127.0.0.1".parse().unwrap()
        } else {
            "::1".parse().unwrap()
        });
    }
    let handler = tokio::spawn(async move {
        let stream = listener.accept().await?;
        let local = canonical_addr(stream.local_addr()?);
        let peer = canonical_addr(stream.remote_addr().unwrap().parse()?);
        handle_client(stream, local, peer, open).await
    });
    (TcpStream::connect(address).await.unwrap(), handler)
}

async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
    stream.write_all(&[5, 2, 2, 0]).await.unwrap();
    let mut response = [0; 2];
    timeout(Duration::from_secs(3), stream.read_exact(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, [5, 0]);
}

async fn request<S: AsyncWrite + Unpin>(stream: &mut S, command: u8, target: &str) {
    let mut bytes = vec![5, command, 0];
    let target = if let Ok(addr) = target.parse() {
        TargetAddr::Ip(addr)
    } else {
        let (host, port) = target.rsplit_once(':').unwrap();
        TargetAddr::Domain(host.to_owned(), port.parse().unwrap())
    };
    bytes.extend(target.to_be_bytes().unwrap());
    stream.write_all(&bytes).await.unwrap();
}

async fn reply<S: AsyncRead + Unpin>(stream: &mut S) -> (u8, SocketAddr) {
    timeout(Duration::from_secs(3), async {
        let mut header = [0; 4];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 5);
        assert_eq!(header[2], 0);
        let TargetAddr::Ip(addr) = read_address(stream, header[3]).await.unwrap() else {
            panic!("replies must use IP addresses");
        };
        (header[1], addr)
    })
    .await
    .unwrap()
}

async fn finish(handler: JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    timeout(Duration::from_secs(3), handler)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn rejects_unoffered_authentication_and_empty_method_list() {
    for methods in [&[5, 1, 2][..], &[5, 0][..]] {
        let (mut stream, handler) = start_proxy("127.0.0.1:0", unused_opener()).await;
        stream.write_all(methods).await.unwrap();
        let mut response = [0; 2];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(response, [5, 255]);
        assert!(finish(handler).await.is_err());
        assert_eq!(
            stream.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}

#[tokio::test]
async fn rejects_invalid_requests_with_socks_replies() {
    for (header, tail, code) in [
        ([5, 2, 0, 1], vec![], 7), // BIND unsupported by current exits
        ([5, 99, 0, 1], vec![], 7),
        ([5, 1, 1, 1], vec![], 1), // reserved byte
        ([4, 1, 0, 1], vec![], 1),
        ([5, 1, 0, 99], vec![], 8),
        ([5, 1, 0, 3], vec![0, 0, 80], 8),      // empty domain
        ([5, 1, 0, 3], vec![1, 255, 0, 80], 8), // invalid UTF-8
    ] {
        let (mut stream, handler) = start_proxy("127.0.0.1:0", unused_opener()).await;
        negotiate(&mut stream).await;
        stream.write_all(&header).await.unwrap();
        stream.write_all(&tail).await.unwrap();
        assert_eq!(reply(&mut stream).await.0, code);
        finish(handler).await.unwrap();
    }
}

#[tokio::test]
async fn connect_preserves_ipv6_domains_pipelined_data_and_half_closes() {
    for target in ["[2606:4700:4700::1111]:443", "does-not-resolve.invalid:443"] {
        let expected = target.to_owned();
        let open: TunnelOpener = Arc::new(move |protocol, target| {
            assert_eq!(protocol, "tcp");
            assert_eq!(target, expected);
            Box::pin(async {
                let (pipe, mut exit) = tokio::io::duplex(1024);
                tokio::spawn(async move {
                    let mut input = vec![];
                    exit.read_to_end(&mut input).await.unwrap();
                    assert_eq!(input, b"request");
                    exit.write_all(b"response after EOF").await.unwrap();
                });
                Ok((
                    Box::new(TestPipe(pipe)) as Box<dyn sillad::Pipe>,
                    "[2606:4700:4700::1111]:443".parse().unwrap(),
                ))
            })
        });
        let (mut stream, handler) = start_proxy("127.0.0.1:0", open).await;
        negotiate(&mut stream).await;
        request(&mut stream, 1, target).await;
        stream.write_all(b"request").await.unwrap();
        stream.shutdown().await.unwrap();
        assert_eq!(reply(&mut stream).await, (0, "[::]:0".parse().unwrap()));
        let mut response = vec![];
        stream.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response after EOF");
        finish(handler).await.unwrap();
    }
}

#[tokio::test]
async fn connect_failures_produce_meaningful_reply_codes() {
    for (error, code) in [
        (
            "exit failed to open tunnel: Connection refused (os error 111)",
            5,
        ),
        ("Network is unreachable (os error 101)", 3),
        ("Proxying to host is not allowed", 2),
        ("failed to resolve DNS", 4),
        ("timeout in TCP dial", 4),
        ("broken mux", 1),
    ] {
        let open: TunnelOpener =
            Arc::new(move |_, _| Box::pin(async move { anyhow::bail!(error) }));
        let (mut stream, handler) = start_proxy("127.0.0.1:0", open).await;
        negotiate(&mut stream).await;
        request(&mut stream, 1, "example.com:443").await;
        assert_eq!(reply(&mut stream).await.0, code);
        assert!(finish(handler).await.is_err());
    }
}

// Exercise short writes explicitly: fast-socks5 1.0's high-level reply helper
// uses write(), whereas this adapter must deliver all reply bytes.
struct ShortWrites(DuplexStream);
impl AsyncRead for ShortWrites {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for ShortWrites {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, &buf[..buf.len().min(1)])
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn handshake_and_replies_survive_short_writes() {
    let (mut app, server) = tokio::io::duplex(128);
    let handler = tokio::spawn(handle_client(
        ShortWrites(server),
        "127.0.0.1:1".parse().unwrap(),
        "127.0.0.1:2".parse().unwrap(),
        unused_opener(),
    ));
    negotiate(&mut app).await;
    request(&mut app, 2, "1.1.1.1:80").await;
    assert_eq!(reply(&mut app).await.0, 7);
    finish(handler).await.unwrap();
}

fn udp_echo_opener() -> (
    TunnelOpener,
    channel::Receiver<String>,
    channel::Receiver<()>,
) {
    let (calls_tx, calls) = channel::unbounded();
    let (closed_tx, closed) = channel::unbounded();
    let open: TunnelOpener = Arc::new(move |protocol, target| {
        assert_eq!(protocol, "udp");
        let calls = calls_tx.clone();
        let closed = closed_tx.clone();
        Box::pin(async move {
            let _ = calls.send(target).await;
            let (pipe, mut exit) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                while let Ok(len) = exit.read_u16_le().await {
                    let mut payload = vec![0; len as usize];
                    if exit.read_exact(&mut payload).await.is_err() {
                        break;
                    }
                    if exit.write_u16_le(len).await.is_err() {
                        break;
                    }
                    if exit.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                let _ = closed.send(()).await;
            });
            Ok((
                Box::new(TestPipe(pipe)) as Box<dyn sillad::Pipe>,
                "1.1.1.1:53".parse().unwrap(),
            ))
        })
    });
    (open, calls, closed)
}

async fn associate(
    bind: &str,
    requested: &str,
    open: TunnelOpener,
) -> (TcpStream, SocketAddr, JoinHandle<anyhow::Result<()>>) {
    let (mut control, handler) = start_proxy(bind, open).await;
    negotiate(&mut control).await;
    request(&mut control, 3, requested).await;
    let (code, relay) = reply(&mut control).await;
    assert_eq!(code, 0);
    assert!(!relay.ip().is_unspecified());
    assert_ne!(relay.port(), 0);
    (control, relay, handler)
}

async fn udp_packet(
    socket: &UdpSocket,
    relay: SocketAddr,
    target: TargetAddr,
    payload: &[u8],
) -> (TargetAddr, Vec<u8>) {
    let mut packet = new_udp_header(target).unwrap();
    packet.extend(payload);
    socket.send_to(&packet, relay).await.unwrap();
    let mut response = vec![0; 65535];
    let (len, source) = timeout(Duration::from_secs(3), socket.recv_from(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source, relay);
    let (frag, target, payload) = parse_udp_request(&response[..len]).await.unwrap();
    assert_eq!(frag, 0);
    (target, payload.to_vec())
}

#[tokio::test]
async fn udp_works_over_both_local_families_and_uses_resolved_reply_addresses() {
    for (bind, unspecified) in [
        ("127.0.0.1:0", "0.0.0.0:0"),
        ("[::1]:0", "[::]:0"),
        ("0.0.0.0:0", "0.0.0.0:0"),
        ("[::]:0", "[::]:0"),
    ] {
        let (open, calls, closed) = udp_echo_opener();
        let (control, relay, handler) = associate(bind, unspecified, open).await;
        let socket = UdpSocket::bind(bind).await.unwrap();
        let (target, payload) = udp_packet(
            &socket,
            relay,
            TargetAddr::Domain("dns.invalid".into(), 53),
            b"hello",
        )
        .await;
        assert_eq!(target, TargetAddr::Ip("1.1.1.1:53".parse().unwrap()));
        assert_eq!(payload, b"hello");
        assert_eq!(calls.recv().await.unwrap(), "dns.invalid:53");
        drop(control);
        finish(handler).await.unwrap();
        timeout(Duration::from_secs(3), closed.recv())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn ipv6_dns_uses_existing_exit_dns_service() {
    let (open, calls, _) = udp_echo_opener();
    let (control, relay, handler) = associate("127.0.0.1:0", "0.0.0.0:0", open).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = TargetAddr::Ip("[2606:4700:4700::1111]:53".parse().unwrap());
    let (target, payload) = udp_packet(&socket, relay, resolver.clone(), b"DNS").await;
    assert_eq!(target, resolver);
    assert_eq!(payload, b"DNS");
    assert_eq!(calls.recv().await.unwrap(), "1.1.1.1:53");
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test]
async fn udp_drops_malformed_fragments_unsupported_ipv6_and_wrong_sources() {
    let (open, calls, _) = udp_echo_opener();
    let (control, relay, handler) = associate("127.0.0.1:0", "0.0.0.0:0", open).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target: SocketAddr = "1.1.1.1:53".parse().unwrap();
    let mut valid = new_udp_header(target).unwrap();
    valid.extend(b"valid");
    // Linux routes all of 127/8 to loopback without configuring aliases.
    #[cfg(target_os = "linux")]
    {
        let wrong_ip = UdpSocket::bind("127.0.0.2:0").await.unwrap();
        wrong_ip.send_to(&valid, relay).await.unwrap();
    }
    let mut reserved = valid.clone();
    reserved[0] = 1;
    let mut fragment = valid.clone();
    fragment[2] = 1;
    let malformed_sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for packet in [
        vec![0],
        vec![0, 0, 0, 3, 255],
        reserved,
        fragment,
        new_udp_header("[2606:4700:4700::1111]:123".parse::<SocketAddr>().unwrap()).unwrap(),
    ] {
        malformed_sender.send_to(&packet, relay).await.unwrap();
    }
    let (_, payload) = udp_packet(&socket, relay, TargetAddr::Ip(target), b"valid").await;
    assert_eq!(payload, b"valid");
    assert_eq!(calls.recv().await.unwrap(), "1.1.1.1:53");
    assert!(calls.try_recv().is_err());
    // A different source port cannot take over after the association is pinned.
    let wrong_port = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    wrong_port.send_to(&valid, relay).await.unwrap();
    udp_packet(&socket, relay, TargetAddr::Ip(target), b"still valid").await;
    let mut buf = [0; 128];
    assert!(
        timeout(Duration::from_millis(50), wrong_port.recv_from(&mut buf))
            .await
            .is_err()
    );
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test]
async fn udp_honors_declared_endpoint_and_rejects_other_ips() {
    let (mut control, handler) = start_proxy("127.0.0.1:0", unused_opener()).await;
    negotiate(&mut control).await;
    request(&mut control, 3, "127.0.0.2:9999").await;
    assert_eq!(reply(&mut control).await.0, 2);
    finish(handler).await.unwrap();

    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let requested = socket.local_addr().unwrap().to_string();
    let (open, _, _) = udp_echo_opener();
    let (control, relay, handler) = associate("127.0.0.1:0", &requested, open).await;
    let target = TargetAddr::Ip("1.1.1.1:53".parse().unwrap());
    let mut packet = new_udp_header(target.clone()).unwrap();
    packet.extend(b"wrong port");
    let wrong = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    wrong.send_to(&packet, relay).await.unwrap();
    assert_eq!(
        udp_packet(&socket, relay, target, b"right port").await.1,
        b"right port"
    );
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test]
async fn failed_udp_flow_can_be_reopened_without_losing_association() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (echo, calls, _) = udp_echo_opener();
    let (failed_tx, failed) = channel::bounded(1);
    let open: TunnelOpener = Arc::new(move |protocol, target| {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            let failed = failed_tx.clone();
            Box::pin(async move {
                failed.send(()).await.unwrap();
                anyhow::bail!("temporary failure")
            })
        } else {
            echo(protocol, target)
        }
    });
    let (control, relay, handler) = associate("127.0.0.1:0", "0.0.0.0:0", open).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = TargetAddr::Ip("1.1.1.1:53".parse().unwrap());
    let mut packet = new_udp_header(target.clone()).unwrap();
    packet.extend(b"first");
    socket.send_to(&packet, relay).await.unwrap();
    failed.recv().await.unwrap();
    // A different flow provides a barrier: the completed failed flow is reaped
    // first by the biased select before accepting the next datagram.
    udp_packet(
        &socket,
        relay,
        TargetAddr::Domain("barrier.invalid".into(), 53),
        b"barrier",
    )
    .await;
    udp_packet(&socket, relay, target, b"retry").await;
    assert_eq!(calls.recv().await.unwrap(), "barrier.invalid:53");
    assert_eq!(calls.recv().await.unwrap(), "1.1.1.1:53");
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test]
async fn blocked_udp_flow_does_not_stall_other_destinations_or_tcp_shutdown() {
    let (echo, _, _) = udp_echo_opener();
    let (blocked_tx, blocked) = channel::bounded(1);
    let open: TunnelOpener = Arc::new(move |protocol, target| {
        if target == "blocked.invalid:53" {
            let started = blocked_tx.clone();
            Box::pin(async move {
                started.send(()).await.unwrap();
                std::future::pending().await
            })
        } else {
            echo(protocol, target)
        }
    });
    let (control, relay, handler) = associate("127.0.0.1:0", "0.0.0.0:0", open).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut packet = new_udp_header(TargetAddr::Domain("blocked.invalid".into(), 53)).unwrap();
    packet.extend(b"blocked");
    socket.send_to(&packet, relay).await.unwrap();
    blocked.recv().await.unwrap();
    for _ in 0..UDP_QUEUE_LEN + 8 {
        socket.send_to(&packet, relay).await.unwrap();
    }
    assert_eq!(
        udp_packet(
            &socket,
            relay,
            TargetAddr::Ip("1.1.1.1:53".parse().unwrap()),
            b"other"
        )
        .await
        .1,
        b"other"
    );
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test]
async fn dual_stack_listener_advertises_ipv4_relay_to_ipv4_clients() {
    let mut listener = sillad::tcp::TcpListener::bind("[::]:0".parse().unwrap())
        .await
        .unwrap();
    let port = listener.local_addr().await.port();
    let (open, _, _) = udp_echo_opener();
    let handler = tokio::spawn(async move {
        let stream = listener.accept().await?;
        let local = canonical_addr(stream.local_addr()?);
        let peer = canonical_addr(stream.remote_addr().unwrap().parse()?);
        handle_client(stream, local, peer, open).await
    });
    let mut control = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    negotiate(&mut control).await;
    request(&mut control, 3, "0.0.0.0:0").await;
    let (code, relay) = reply(&mut control).await;
    assert_eq!(code, 0);
    assert_eq!(relay.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert_eq!(
        udp_packet(
            &socket,
            relay,
            TargetAddr::Ip("1.1.1.1:53".parse().unwrap()),
            b"dual stack"
        )
        .await
        .1,
        b"dual stack"
    );
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test]
async fn udp_literal_replies_preserve_fake_dns_identity() {
    let (open, _, _) = udp_echo_opener();
    let (control, relay, handler) = associate("127.0.0.1:0", "0.0.0.0:0", open).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = TargetAddr::Ip("198.18.0.1:123".parse().unwrap());
    let (reply, payload) = udp_packet(&socket, relay, target.clone(), b"fake DNS").await;
    assert_eq!(reply, target);
    assert_eq!(payload, b"fake DNS");
    drop(control);
    finish(handler).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stalled_handshake_and_connect_are_bounded() {
    let (mut app, server) = tokio::io::duplex(1024);
    let local = "127.0.0.1:1".parse().unwrap();
    let peer = "127.0.0.1:2".parse().unwrap();
    let handler = tokio::spawn(handle_client(server, local, peer, unused_opener()));
    // The client stops in the middle of its greeting.
    app.write_all(&[5]).await.unwrap();
    assert!(
        handler
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("handshake timed out")
    );

    let (mut app, server) = tokio::io::duplex(1024);
    let open: TunnelOpener = Arc::new(|_, _| Box::pin(std::future::pending()));
    let handler = tokio::spawn(handle_client(server, local, peer, open));
    negotiate(&mut app).await;
    request(&mut app, 1, "example.com:443").await;
    // Read without a shorter test timeout, allowing paused time to advance
    // through the actual connection deadline.
    let mut response = [0; 10];
    app.read_exact(&mut response).await.unwrap();
    assert_eq!(&response[..4], &[5, 4, 0, 1]);
    assert!(handler.await.unwrap().is_err());
}

#[tokio::test]
async fn udp_idle_timeout_counts_traffic_in_either_direction() {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let (pipe, mut exit) = tokio::io::duplex(1024);
    let pipe = Arc::new(std::sync::Mutex::new(Some(pipe)));
    let open: TunnelOpener = Arc::new(move |_, _| {
        let pipe = pipe.lock().unwrap().take().unwrap();
        Box::pin(async move {
            Ok((
                Box::new(TestPipe(pipe)) as Box<dyn sillad::Pipe>,
                "1.1.1.1:123".parse().unwrap(),
            ))
        })
    });
    let (tx, rx) = channel::bounded(32);
    tokio::time::pause();
    let handler = tokio::spawn(run_udp_tunnel(
        TargetAddr::Ip("1.1.1.1:123".parse().unwrap()),
        socket,
        "127.0.0.1:10000".parse().unwrap(),
        rx,
        open,
    ));
    for _ in 0..3 {
        tx.send(vec![42]).await.unwrap();
        assert_eq!(exit.read_u16_le().await.unwrap(), 1);
        assert_eq!(exit.read_u8().await.unwrap(), 42);
        tokio::time::advance(Duration::from_secs(40)).await;
        assert!(!handler.is_finished(), "active one-way UDP must not expire");
    }
    tokio::time::advance(UDP_IDLE_TIMEOUT).await;
    assert!(
        handler
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("idle SOCKS UDP flow")
    );
}

#[tokio::test]
async fn connect_integrates_with_session_passthrough_for_both_families() {
    for bind in ["127.0.0.1:0", "[::1]:0"] {
        let destination = tokio::net::TcpListener::bind(bind).await.unwrap();
        let remote = destination.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = destination.accept().await.unwrap();
            let mut input = [0; 4];
            stream.read_exact(&mut input).await.unwrap();
            assert_eq!(&input, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });
        let ctx = AnyCtx::new(
            serde_json::from_value::<Config>(serde_json::json!({
                "exit_constraint": "auto", "dry_run": true, "cache": ":memory:"
            }))
            .unwrap(),
        );
        let open: TunnelOpener = Arc::new(move |protocol, target| {
            let ctx = ctx.clone();
            Box::pin(async move { open_conn_with_addr(&ctx, protocol, &target).await })
        });
        let (mut stream, handler) = start_proxy("127.0.0.1:0", open).await;
        negotiate(&mut stream).await;
        request(&mut stream, 1, &remote.to_string()).await;
        let (code, bound) = reply(&mut stream).await;
        assert_eq!(code, 0);
        assert_eq!(bound.is_ipv6(), remote.is_ipv6());
        stream.write_all(b"ping").await.unwrap();
        let mut response = [0; 4];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        drop(stream);
        finish(handler).await.unwrap();
        echo.await.unwrap();
    }
}

#[tokio::test]
async fn udp_flow_limit_bounds_pending_opens_and_cancels_them_on_close() {
    struct Opening(channel::Sender<()>);
    impl Drop for Opening {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }
    let (started_tx, started) = channel::unbounded();
    let (closed_tx, closed) = channel::unbounded();
    let open: TunnelOpener = Arc::new(move |_, target| {
        let started = started_tx.clone();
        let closed = closed_tx.clone();
        Box::pin(async move {
            let _opening = Opening(closed);
            started.send(target).await.unwrap();
            std::future::pending().await
        })
    });
    let (control, relay, handler) = associate("127.0.0.1:0", "0.0.0.0:0", open).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for index in 0..MAX_UDP_FLOWS {
        let target = format!("flow-{index}.invalid");
        let packet = new_udp_header(TargetAddr::Domain(target.clone(), 123)).unwrap();
        socket.send_to(&packet, relay).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(3), started.recv())
                .await
                .unwrap()
                .unwrap(),
            format!("{target}:123")
        );
    }
    let excess = new_udp_header(TargetAddr::Domain("excess.invalid".into(), 123)).unwrap();
    socket.send_to(&excess, relay).await.unwrap();
    assert!(
        timeout(Duration::from_millis(50), started.recv())
            .await
            .is_err()
    );
    drop(control);
    finish(handler).await.unwrap();
    for _ in 0..MAX_UDP_FLOWS {
        timeout(Duration::from_secs(3), closed.recv())
            .await
            .unwrap()
            .unwrap();
    }
}
