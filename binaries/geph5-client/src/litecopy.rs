use futures_util::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub async fn litecopy<R, W>(mut reader: R, mut writer: W) -> Result<u64, std::io::Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut n = 0;
    loop {
        let val = async_io_bufpool::pooled_read(&mut reader).await?;
        if val.is_empty() {
            return Ok(n);
        }
        writer.write_all(&val).await?;
        n += val.len() as u64;
    }
}
