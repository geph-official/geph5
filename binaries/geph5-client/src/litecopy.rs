use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Copy one direction of a duplex stream until EOF, then propagate the EOF by
/// shutting down the writer (half-close). Callers run the two directions with
/// `try_join` so that one side reaching EOF does not tear down the other —
/// otherwise a peer that half-closes after sending its request would truncate
/// the response still streaming back.
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
            None => {
                // Forward the half-close so the peer learns this direction
                // ended; ignore the error if it is already gone.
                let _ = writer.shutdown().await;
                return Ok(n);
            }
        }
    }
}
