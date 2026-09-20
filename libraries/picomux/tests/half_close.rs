use futures_util::future::try_join;
use geph5_rt::block_on;
use picomux::{PicoMux, Stream};
use std::{io, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn pair() -> (PicoMux, PicoMux) {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    (PicoMux::new(a_read, a_write), PicoMux::new(b_read, b_write))
}

async fn streams(a: &PicoMux, b: &PicoMux) -> (Stream, Stream) {
    try_join(a.open(b"half-close"), b.accept()).await.unwrap()
}

#[test]
fn response_after_request_eof_crosses_flow_control_windows() {
    block_on(async {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (a, b) = pair();
            let (mut client, mut server) = streams(&a, &b).await;
            let request = vec![0x37; 128 * 1024 + 19];
            // Exceeds both the duplex buffers and picomux's maximum window.
            let response = vec![0x91; 1501 * 8192 + 23];

            let client_task = async {
                client.write_all(&request).await?;
                client.shutdown().await?;
                client.shutdown().await?;
                assert!(client.write_all(b"after shutdown").await.is_err());

                let mut received = Vec::new();
                client.read_to_end(&mut received).await?;
                assert_eq!(received.len(), response.len());
                assert!(received == response, "response bytes differ");
                Ok::<_, io::Error>(())
            };
            let server_task = async {
                let mut received = Vec::new();
                server.read_to_end(&mut received).await?;
                assert_eq!(received.len(), request.len());
                assert!(received == request, "request bytes differ");

                server.write_all(&response).await?;
                server.shutdown().await?;
                Ok::<_, io::Error>(())
            };
            try_join(client_task, server_task).await.unwrap();
        })
        .await
        .expect("half-close request/response stalled");
    });
}

#[test]
fn simultaneous_shutdown_preserves_both_directions() {
    block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (a, b) = pair();
            let (client, server) = streams(&a, &b).await;

            async fn exchange(stream: Stream, sent: u8, expected: u8) -> io::Result<()> {
                let (mut read, mut write) = tokio::io::split(stream);
                let sender = async {
                    write.write_all(&vec![sent; 256 * 1024]).await?;
                    write.shutdown().await
                };
                let receiver = async {
                    let mut bytes = Vec::new();
                    read.read_to_end(&mut bytes).await?;
                    assert_eq!(bytes.len(), 256 * 1024);
                    assert!(bytes.iter().all(|byte| *byte == expected));
                    Ok::<_, io::Error>(())
                };
                try_join(sender, receiver).await?;
                Ok(())
            }

            try_join(exchange(client, 17, 23), exchange(server, 23, 17))
                .await
                .unwrap();
        })
        .await
        .expect("simultaneous shutdown stalled");
    });
}

#[test]
fn dropping_stream_drains_writes_and_keeps_mux_usable() {
    block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (a, b) = pair();
            for shutdown in [false, true] {
                let (mut client, mut server) = streams(&a, &b).await;
                let payload = vec![42; 16 * 1024];
                client.write_all(&payload).await.unwrap();
                if shutdown {
                    client.shutdown().await.unwrap();
                }
                drop(client);

                let mut received = Vec::new();
                server.read_to_end(&mut received).await.unwrap();
                assert_eq!(received.len(), payload.len());
                assert!(received == payload, "buffered bytes differ");
            }

            let (mut client, mut server) = streams(&a, &b).await;
            client.shutdown().await.unwrap();
            assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
            server.write_all(b"response").await.unwrap();
            server.shutdown().await.unwrap();
            let mut received = Vec::new();
            client.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, b"response");
        })
        .await
        .expect("stream drop or subsequent stream stalled");
    });
}

#[test]
fn shutdown_reports_closed_transport() {
    block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (a, b) = pair();
            let (mut client, server) = streams(&a, &b).await;
            drop(server);
            drop(b);
            a.wait_until_dead().await.unwrap_err();
            assert!(client.shutdown().await.is_err());
        })
        .await
        .expect("shutdown did not report the closed transport");
    });
}
