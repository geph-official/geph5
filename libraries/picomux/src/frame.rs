use bytemuck::{Pod, Zeroable};
use bytes::Bytes;
use futures_util::{AsyncRead, AsyncReadExt};

#[derive(Clone)]
pub struct Frame {
    pub header: Header,
    pub body: Bytes,
}

impl Frame {
    /// Read a frame from an async reader.
    pub async fn read(mut rdr: impl AsyncRead + Unpin) -> std::io::Result<Self> {
        let mut header_buf = [0; std::mem::size_of::<Frame>()];
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
        let mut buf = vec![0; self.body.len() + std::mem::size_of::<Frame>()];
        buf[..std::mem::size_of::<Frame>()].copy_from_slice(bytemuck::bytes_of(&self.header));
        buf[std::mem::size_of::<Frame>()..].copy_from_slice(&self.body);
        buf.into()
    }
}

#[derive(Clone, Copy, Pod, PartialEq, Zeroable)]
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
