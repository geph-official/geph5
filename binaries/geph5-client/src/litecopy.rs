use futures_util::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub async fn litecopy<R, W>(mut reader: R, mut writer: W) -> Result<u64, std::io::Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let val = async_io_bufpool::pooled_read(&mut reader).await?;
        writer.write_all(&val).await?;
    }
}
