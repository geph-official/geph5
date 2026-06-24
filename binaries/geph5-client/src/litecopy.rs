use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub async fn litecopy<R, W>(mut reader: R, mut writer: W) -> Result<u64, std::io::Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut n = 0;
    loop {
        // `None` signals EOF in async-io-bufpool 0.2.
        match geph5_rt::pooled_read(&mut reader, 8192).await? {
            Some(val) => {
                writer.write_all(&val).await?;
                n += val.len() as u64;
            }
            None => return Ok(n),
        }
    }
}
