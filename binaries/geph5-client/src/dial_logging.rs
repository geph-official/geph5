use std::io;

use async_trait::async_trait;
use sillad::{Pipe, dialer::Dialer};

pub fn logged<D: Dialer>(
    name: &'static str,
    route_subtree_json: String,
    inner: D,
) -> LoggedDialer<D> {
    LoggedDialer {
        name,
        route_subtree_json,
        inner,
    }
}

pub struct LoggedDialer<D: Dialer> {
    name: &'static str,
    route_subtree_json: String,
    inner: D,
}

#[async_trait]
impl<D: Dialer> Dialer for LoggedDialer<D> {
    type P = D::P;

    async fn dial(&self) -> io::Result<Self::P> {
        match self.inner.dial().await {
            Ok(pipe) => {
                tracing::debug!(
                    stage = self.name,
                    remote_addr = pipe.remote_addr(),
                    "dial stage succeeded"
                );
                Ok(pipe)
            }
            Err(err) => {
                tracing::debug!(
                    stage = self.name,
                    route_subtree = %self.route_subtree_json,
                    err = debug(&err),
                    "dial stage failed"
                );
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use async_trait::async_trait;
    use futures_util::{AsyncRead, AsyncWrite};
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    #[derive(Clone)]
    struct TestPipe {
        remote_addr: Option<&'static str>,
    }

    impl AsyncRead for TestPipe {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }
    }

    impl AsyncWrite for TestPipe {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Pipe for TestPipe {
        fn protocol(&self) -> &str {
            "test"
        }

        fn remote_addr(&self) -> Option<&str> {
            self.remote_addr
        }
    }

    struct OkDialer {
        remote_addr: Option<&'static str>,
    }

    #[async_trait]
    impl Dialer for OkDialer {
        type P = TestPipe;

        async fn dial(&self) -> io::Result<Self::P> {
            Ok(TestPipe {
                remote_addr: self.remote_addr,
            })
        }
    }

    struct ErrDialer;

    #[async_trait]
    impl Dialer for ErrDialer {
        type P = TestPipe;

        async fn dial(&self) -> io::Result<Self::P> {
            Err(io::Error::other("boom"))
        }
    }

    #[derive(Clone, Default)]
    struct TestWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(f: impl FnOnce() -> T) -> (String, T) {
        let writer = TestWriter::default();
        let captured = writer.buf.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .compact()
                .with_writer(move || writer.clone()),
        );

        let result = tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        (logs, result)
    }

    #[test]
    fn logged_dialer_logs_success() {
        let (logs, result) = capture_logs(|| {
            let dialer = logged(
                "tcp",
                r#"{"tcp":"127.0.0.1:9000"}"#.to_string(),
                OkDialer {
                    remote_addr: Some("127.0.0.1:9000"),
                },
            );
            smolscale::block_on(async move { dialer.dial().await })
        });

        assert!(result.is_ok());
        assert!(logs.contains("dial stage succeeded"));
        assert!(logs.contains("tcp"));
        assert!(logs.contains("127.0.0.1:9000"));
    }

    #[test]
    fn logged_dialer_logs_failure() {
        let (logs, result) = capture_logs(|| {
            let dialer = logged(
                "tls",
                r#"{"plain_tls":{"lower":"..."}}"#.to_string(),
                ErrDialer,
            );
            smolscale::block_on(async move { dialer.dial().await })
        });

        assert!(result.is_err());
        assert!(logs.contains("dial stage failed"));
        assert!(logs.contains("tls"));
        assert!(logs.contains(r#"{"plain_tls":{"lower":"..."}}"#));
        assert!(logs.contains("boom"));
    }

    #[test]
    fn nested_logged_dialers_cascade_failures() {
        let (logs, result) = capture_logs(|| {
            let dialer = logged(
                "overall",
                r#"{"overall":true}"#.to_string(),
                logged(
                    "sosistab3",
                    r#"{"sosistab3":{"lower":"..."}}"#.to_string(),
                    logged("tcp", r#"{"tcp":"127.0.0.1:9000"}"#.to_string(), ErrDialer),
                ),
            );
            smolscale::block_on(async move { dialer.dial().await })
        });

        assert!(result.is_err());
        assert!(logs.contains("tcp"));
        assert!(logs.contains("sosistab3"));
        assert!(logs.contains("overall"));
    }

    #[test]
    fn nested_logged_dialers_log_layered_success() {
        let (logs, result) = capture_logs(|| {
            let dialer = logged(
                "overall",
                r#"{"overall":true}"#.to_string(),
                logged(
                    "sosistab3",
                    r#"{"sosistab3":{"lower":"..."}}"#.to_string(),
                    logged(
                        "tcp",
                        r#"{"tcp":"127.0.0.1:9000"}"#.to_string(),
                        OkDialer {
                            remote_addr: Some("127.0.0.1:9000"),
                        },
                    ),
                ),
            );
            smolscale::block_on(async move { dialer.dial().await })
        });

        assert!(result.is_ok());
        assert!(logs.contains("tcp"));
        assert!(logs.contains("sosistab3"));
        assert!(logs.contains("overall"));
    }
}
