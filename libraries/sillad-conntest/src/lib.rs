use std::time::{Duration, Instant};

use async_io::Timer;
use async_task::Task;
use async_trait::async_trait;
use futures_lite::FutureExt as _;
use futures_util::{AsyncReadExt, AsyncWriteExt};
use rand::{Rng, RngCore};
use sillad::{dialer::Dialer, listener::Listener, Pipe};

/// Wraps an underlying dialer with a connection quality test.
pub struct ConnTestDialer<D: Dialer> {
    pub inner: D,
    pub ping_count: usize,
}

#[async_trait]
impl<D: Dialer> Dialer for ConnTestDialer<D> {
    type P = D::P;

    async fn dial(&self) -> std::io::Result<Self::P> {
        let mut pipe = self.inner.dial().await?;
        for index in 0..self.ping_count {
            let start = Instant::now();
            // Pick a random payload size (nonzero)
            let size = rand::rng().random_range(1..50000u16);
            // Tell the server the payload size.
            pipe.write_all(&size.to_be_bytes()).await?;
            // Prepare and send a random payload.
            let mut buf = vec![0u8; size as usize];
            rand::rng().fill_bytes(&mut buf);
            pipe.write_all(&buf).await?;
            // Read back the echoed payload.
            let mut echo = vec![0u8; size as usize];
            pipe.read_exact(&mut echo).await?;
            let remote_addr = pipe.remote_addr();
            tracing::debug!(
                elapsed = debug(start.elapsed()),
                total_count = self.ping_count,
                index,
                remote_addr = debug(remote_addr),
                "ping completed"
            );
            if buf != echo {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ping returned incorrect data",
                ));
            }
        }
        // Termination message: a 0 length indicates end of testing.
        pipe.write_all(&[0u8; 2]).await?;
        Ok(pipe)
    }
}

/// Wraps an underlying listener with a connection quality test.
pub struct ConnTestListener<L: Listener> {
    recv_conn: tachyonix::Receiver<L::P>,
    _task: Task<std::io::Result<()>>,
}

impl<L: Listener> ConnTestListener<L> {
    pub fn new(mut listener: L) -> Self {
        // Create a channel for passing successfully tested connections.
        let (send_conn, recv_conn) = tachyonix::channel(1);
        // Spawn a background task that loops over accepted connections.
        let task = smolscale::spawn(async move {
            loop {
                // Accept a new connection from the underlying listener.
                let mut conn = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to accept connection: {:?}", e);
                        continue;
                    }
                };
                let send_conn = send_conn.clone();
                // For each accepted connection, spawn a task to perform the ping test.
                smolscale::spawn::<std::io::Result<()>>(async move {
                    let inner = async {
                        loop {
                            let mut size_buf = [0u8; 2];
                            conn.read_exact(&mut size_buf).await?;
                            let size = u16::from_be_bytes(size_buf);
                            // A zero size means the client has finished pinging.
                            if size == 0 {
                                let _ = send_conn.send(conn).await;
                                return Ok(());
                            }
                            let mut payload = vec![0u8; size as usize];
                            conn.read_exact(&mut payload).await?;
                            conn.write_all(&payload).await?;
                        }
                    };
                    inner
                        .or(async {
                            Timer::after(Duration::from_secs(30)).await;
                            Ok(())
                        })
                        .await
                })
                .detach();
            }
            // This point is never reached.
            #[allow(unreachable_code)]
            Ok(())
        });
        Self {
            recv_conn,
            _task: task,
        }
    }
}

#[async_trait]
impl<L: Listener> Listener for ConnTestListener<L> {
    type P = L::P;

    async fn accept(&mut self) -> std::io::Result<Self::P> {
        // Wait for a connection that passed the ping test.
        self.recv_conn.recv().await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "background task is done")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_lite::{AsyncReadExt, AsyncWriteExt};

    use sillad::tcp::{TcpDialer, TcpListener};
    use smolscale::spawn;
    use std::io;
    use std::net::SocketAddr;

    // If your TCP types are defined in another module, adjust these imports accordingly.
    // For example:
    // use crate::{TcpListener, TcpDialer};

    /// This unit test creates a TCP listener (wrapped by `ConnTestListener`) that
    /// echoes incoming data. The client uses `ConnTestDialer` to perform several
    /// ping rounds before using the connection. The test then verifies that a test
    /// message is echoed back correctly.
    #[test]
    fn test_successful_ping() -> io::Result<()> {
        async_io::block_on(async {
            // Bind a TCP listener to an ephemeral port on localhost.
            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let tcp_listener = TcpListener::bind(addr).await?;
            let local_addr = tcp_listener.local_addr().await;

            // Wrap the TCP listener with ConnTestListener.
            let mut conn_test_listener = ConnTestListener::new(tcp_listener);

            // Spawn a background task that, once the ping test is complete,
            // performs an echo for any additional messages.
            let server_handle = spawn(async move {
                // Accept the connection that passed the ping test.
                let mut conn = conn_test_listener.accept().await?;
                let mut buf = [0u8; 1024];
                loop {
                    let n = conn.read(&mut buf).await?;
                    if n == 0 {
                        break; // Connection closed.
                    }
                    conn.write_all(&buf[..n]).await?;
                }
                Ok::<(), io::Error>(())
            });

            // Create a TCP dialer pointed at the server’s address.
            let tcp_dialer = TcpDialer {
                dest_addr: local_addr,
            };

            // Wrap the TCP dialer with ConnTestDialer (performing, for example, 3 ping rounds).
            let conn_test_dialer = ConnTestDialer {
                inner: tcp_dialer,
                ping_count: 3,
            };

            // Dial to the server. This will perform the ping test internally.
            let mut client_pipe = conn_test_dialer.dial().await?;

            // Send a test message and expect an echo.
            let test_message = b"hello, unit test!";
            client_pipe.write_all(test_message).await?;
            let mut buf = vec![0u8; test_message.len()];
            client_pipe.read_exact(&mut buf).await?;
            assert_eq!(
                &buf, test_message,
                "the echoed message should match the sent message"
            );

            // Clean up.
            drop(client_pipe);
            server_handle.await?;
            Ok(())
        })
    }

    /// This unit test simulates a server that deliberately corrupts the ping echo.
    /// As a result, the `ConnTestDialer` should detect the invalid data and fail.
    #[test]
    fn test_failed_ping() -> io::Result<()> {
        async_io::block_on(async {
            // Bind a TCP listener to an ephemeral port.
            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let mut tcp_listener = TcpListener::bind(addr).await?;
            let local_addr = tcp_listener.local_addr().await;

            // Spawn a server task that corrupts each ping echo.
            let server_handle = spawn(async move {
                let mut conn = tcp_listener.accept().await?;
                loop {
                    // Read the two-byte payload size.
                    let mut size_buf = [0u8; 2];
                    if conn.read_exact(&mut size_buf).await.is_err() {
                        break; // Connection closed.
                    }
                    let size = u16::from_be_bytes(size_buf);
                    if size == 0 {
                        break; // Termination message.
                    }
                    // Read the payload.
                    let mut payload = vec![0u8; size as usize];
                    conn.read_exact(&mut payload).await?;
                    // Corrupt the payload (flip the first byte, if any).
                    if !payload.is_empty() {
                        payload[0] = payload[0].wrapping_add(1);
                    }
                    // Send the corrupted payload back.
                    conn.write_all(&payload).await?;
                }
                Ok::<(), io::Error>(())
            });

            // Create a TCP dialer pointed at the server’s address.
            let tcp_dialer = TcpDialer {
                dest_addr: local_addr,
            };

            // Wrap the TCP dialer with ConnTestDialer (using 3 ping rounds).
            let conn_test_dialer = ConnTestDialer {
                inner: tcp_dialer,
                ping_count: 3,
            };

            // Attempt to dial to the server.
            // Since the server corrupts the echoed pings, the dial should return an error.
            let result = conn_test_dialer.dial().await;
            assert!(
                result.is_err(),
                "dialing should fail due to corrupted ping echoes"
            );

            let _ = server_handle.await;
            Ok(())
        })
    }
}
