//! Windows named-pipe transport.
//!
//! This is the Windows analogue of [`crate::unix`]: a filesystem-namespaced,
//! local-only, access-controlled stream transport, used for the geph daemon's
//! control plane instead of squat-prone loopback TCP.
//!
//! Now that sillad lives in the tokio-io ecosystem, tokio's own
//! `named_pipe::{NamedPipeServer, NamedPipeClient}` already implement
//! `tokio::io::{AsyncRead, AsyncWrite}`, so this is a thin delegating wrapper —
//! no async-compat bridge is needed. The only piece tokio doesn't expose is a
//! security descriptor on the server instance, so instances are still created by
//! hand via `CreateNamedPipeW` with a caller-supplied SDDL string (the
//! named-pipe equivalent of `chmod 0666`), then wrapped with
//! `NamedPipeServer::from_raw_handle`. Handle creation must happen inside a
//! tokio runtime context (it registers with tokio's IOCP); sillad always runs on
//! the global runtime, so that holds.

use std::{
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use pin_project::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
use windows_sys::Win32::{
    Foundation::{INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX},
    System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES,
    },
};

const ERROR_PIPE_BUSY: i32 = 231;
const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

/// An SDDL string granting full control to LocalSystem and Administrators, plus
/// read/write to all authenticated users — the named-pipe equivalent of the unix
/// daemon socket's `0666`, so an unprivileged CLI/GUI can reach a pipe created by
/// the (service) daemon.
pub const SDDL_ALLOW_AUTHENTICATED: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Create a single named-pipe server instance via the raw Win32 API so we can
/// attach a security descriptor. Must be called inside a tokio reactor context
/// (`from_raw_handle` registers the handle with tokio's IOCP).
fn create_instance(
    name_w: &[u16],
    sddl_w: Option<&[u16]>,
    first: bool,
) -> io::Result<NamedPipeServer> {
    // Build SECURITY_ATTRIBUTES from the SDDL string, if any. `sa` must outlive
    // the CreateNamedPipeW call below.
    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let sa;
    let psa: *const SECURITY_ATTRIBUTES = if let Some(sddl) = sddl_w {
        // SAFETY: sddl is a NUL-terminated wide string; psd receives an owned
        // descriptor we LocalFree below.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        &sa
    } else {
        std::ptr::null()
    };

    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        // Fail (rather than silently sharing the name) if some other process
        // already owns this pipe name — basic anti-squatting.
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }

    // SAFETY: name_w is NUL-terminated; psa is either null or points at `sa`,
    // which lives until after this call.
    let handle = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            psa,
        )
    };
    let create_err = io::Error::last_os_error();
    if !psd.is_null() {
        // SAFETY: psd was allocated by ConvertStringSecurityDescriptor... above.
        unsafe { LocalFree(psd as _) };
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(create_err);
    }
    // SAFETY: a freshly created, overlapped named-pipe server handle we own, and
    // we are inside a tokio reactor context.
    unsafe { NamedPipeServer::from_raw_handle(handle as _) }
}

/// Listens for incoming connections on a Windows named pipe.
///
/// Like the unix listener, a named pipe lives in a system namespace
/// (`\\.\pipe\<name>`) and is access-controlled (here, by the SDDL passed to
/// [`bind`](Self::bind)) rather than reachable by anyone who can open a TCP port.
pub struct NamedPipeListener {
    name_w: Vec<u16>,
    sddl_w: Option<Vec<u16>>,
    next: Option<NamedPipeServer>,
    first: bool,
}

impl NamedPipeListener {
    /// Bind a listener to a pipe name such as `\\.\pipe\geph-daemon-control`.
    ///
    /// `sddl`, when set, is a Win32 SDDL security-descriptor string applied to
    /// every instance (e.g. [`SDDL_ALLOW_AUTHENTICATED`]); when `None`, the pipe
    /// gets the default DACL (creator + system only).
    ///
    /// Binding is cheap and infallible: the first instance (with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` for anti-squatting) is created on the
    /// first `accept`, since handle creation must run inside the tokio reactor.
    pub fn bind(name: impl AsRef<str>, sddl: Option<&str>) -> io::Result<Self> {
        Ok(Self {
            name_w: to_wide(name.as_ref()),
            sddl_w: sddl.map(to_wide),
            next: None,
            first: true,
        })
    }

    fn make_instance(&mut self) -> io::Result<NamedPipeServer> {
        let first = self.first;
        self.first = false;
        create_instance(&self.name_w, self.sddl_w.as_deref(), first)
    }
}

#[async_trait]
impl crate::listener::Listener for NamedPipeListener {
    type P = NamedPipe;

    async fn accept(&mut self) -> io::Result<Self::P> {
        let server = match self.next.take() {
            Some(s) => s,
            None => self.make_instance()?,
        };
        server.connect().await?;
        // Pre-create the instance the next accept() will hand out, so there is
        // always a server listening between connections.
        self.next = Some(self.make_instance()?);
        Ok(NamedPipe::Server(server))
    }
}

/// Connects to a [`NamedPipeListener`].
pub struct NamedPipeDialer {
    /// Pipe name, e.g. `\\.\pipe\geph-daemon-control`.
    pub name: String,
}

#[async_trait]
impl crate::dialer::Dialer for NamedPipeDialer {
    type P = NamedPipe;

    async fn dial(&self) -> io::Result<Self::P> {
        loop {
            match ClientOptions::new().read(true).write(true).open(&self.name) {
                Ok(client) => return Ok(NamedPipe::Client(client)),
                // All instances are momentarily busy serving other clients;
                // wait for one to free up, like the Win32 WaitNamedPipe loop.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// A connected named-pipe stream (either end), delegating tokio-io directly to
/// the underlying named-pipe handle.
#[pin_project(project = NamedPipeProj)]
pub enum NamedPipe {
    Server(#[pin] NamedPipeServer),
    Client(#[pin] NamedPipeClient),
}

impl AsyncRead for NamedPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            NamedPipeProj::Server(s) => s.poll_read(cx, buf),
            NamedPipeProj::Client(c) => c.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for NamedPipe {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.project() {
            NamedPipeProj::Server(s) => s.poll_write(cx, buf),
            NamedPipeProj::Client(c) => c.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            NamedPipeProj::Server(s) => s.poll_flush(cx),
            NamedPipeProj::Client(c) => c.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            NamedPipeProj::Server(s) => s.poll_shutdown(cx),
            NamedPipeProj::Client(c) => c.poll_shutdown(cx),
        }
    }
}

impl crate::Pipe for NamedPipe {
    fn protocol(&self) -> &str {
        "windows-pipe"
    }

    fn remote_addr(&self) -> Option<&str> {
        None
    }
}
