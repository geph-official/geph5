use std::{
    os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    pin::Pin,
};

use async_io::Async;
use async_trait::async_trait;
use futures_lite::{AsyncRead, AsyncWrite};
use pin_project::pin_project;

use crate::{
    Pipe,
    dialer::Dialer,
    listener::Listener,
};

/// A UnixListener listens for incoming connections on a filesystem path (AF_UNIX).
///
/// Binding unlinks any pre-existing file at the path so a stale socket left
/// behind by a previous run does not cause `EADDRINUSE`. Callers that need
/// stricter semantics should check for and refuse to overwrite the path
/// themselves before calling `bind`.
pub struct UnixListener {
    inner: Async<StdUnixListener>,
    path: PathBuf,
}

impl UnixListener {
    /// Bind a new listener to the given filesystem path.
    pub async fn bind(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let _ = std::fs::remove_file(&path);
        let listener = StdUnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            inner: Async::new(listener)?,
            path,
        })
    }

    /// The filesystem path this listener is bound to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl Listener for UnixListener {
    type P = UnixPipe;

    async fn accept(&mut self) -> std::io::Result<Self::P> {
        let (conn, _) = self.inner.accept().await?;
        Ok(UnixPipe(conn))
    }
}

/// A UnixDialer connects to a `UnixListener` at a given path.
pub struct UnixDialer {
    pub path: PathBuf,
}

#[async_trait]
impl Dialer for UnixDialer {
    type P = UnixPipe;

    async fn dial(&self) -> std::io::Result<Self::P> {
        let inner = Async::<StdUnixStream>::connect(&self.path).await?;
        Ok(UnixPipe(inner))
    }
}

#[pin_project]
pub struct UnixPipe(#[pin] Async<StdUnixStream>);

impl AsyncRead for UnixPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().0.poll_read(cx, buf)
    }
}

impl AsyncWrite for UnixPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.project().0.poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().0.poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.project().0.poll_close(cx)
    }
}

impl Pipe for UnixPipe {
    fn protocol(&self) -> &str {
        "unix"
    }

    fn remote_addr(&self) -> Option<&str> {
        None
    }
}
