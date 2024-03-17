use bytemuck::{Pod, Zeroable};
use bytes::Bytes;
use futures_util::{AsyncRead, AsyncReadExt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Frame {
    pub header: Header,
    pub body: Bytes,
}

impl Frame {
    /// Create an empty frame with the given stream ID and command.
    pub fn new_empty(stream_id: u32, command: u8) -> Self {
        Self {
            header: Header {
                version: 1,
                command,
                body_len: 0,
                stream_id,
            },
            body: Bytes::new(),
        }
    }

    /// Create an frame with the given stream ID, command, and body
    pub fn new(stream_id: u32, command: u8, body: &[u8]) -> Self {
        Self {
            header: Header {
                version: 1,
                command,
                body_len: body.len() as _,
                stream_id,
            },
            body: Bytes::copy_from_slice(body),
        }
    }

    /// Read a frame from an async reader.
    pub async fn read(mut rdr: impl AsyncRead + Unpin) -> std::io::Result<Self> {
        let mut header_buf = [0; std::mem::size_of::<Header>()];
        rdr.read_exact(&mut header_buf).await?;
        let header: Header = bytemuck::cast(header_buf);
        let len = header.body_len as usize;
        let mut body = vec![0; len];
        rdr.read_exact(&mut body).await?;
        Ok(Self {
            header,
            body: body.into(),
        })
    }

    /// The bytes representation of the frame.
    pub fn bytes(&self) -> Bytes {
        let mut buf = vec![0; self.body.len() + std::mem::size_of::<Header>()];
        buf[..std::mem::size_of::<Header>()].copy_from_slice(bytemuck::bytes_of(&self.header));
        buf[std::mem::size_of::<Header>()..].copy_from_slice(&self.body);
        buf.into()
    }
}

#[derive(Clone, Copy, Pod, PartialEq, Zeroable, Debug)]
#[repr(C)]
pub struct Header {
    pub version: u8,
    pub command: u8,
    pub body_len: u16,
    pub stream_id: u32,
}

pub const CMD_SYN: u8 = 0;
pub const CMD_FIN: u8 = 1;
pub const CMD_PSH: u8 = 2;
pub const CMD_NOP: u8 = 3;
pub const CMD_MORE: u8 = 4;

pub const CMD_PING: u8 = 0xa0;
pub const CMD_PONG: u8 = 0xa1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PingInfo {
    pub next_ping_in_ms: u32,
}
