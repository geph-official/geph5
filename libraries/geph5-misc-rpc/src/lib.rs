use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod bridge;
pub mod client_control;
pub mod exit;

/// A helper function to write a length-prepended value into an AsyncWrite.
pub async fn write_prepend_length<W: AsyncWrite + Unpin>(
    value: &[u8],
    mut out: W,
) -> std::io::Result<()> {
    let len = value.len() as u32;
    let len_buf = len.to_be_bytes();

    out.write_all(&len_buf).await?;
    out.write_all(value).await?;
    out.flush().await
}

/// A helper function to read a length-prepended value from an AsyncRead.
pub async fn read_prepend_length<R: AsyncRead + Unpin>(mut input: R) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    input.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 100_000 {
        return Err(std::io::Error::other(
            "cannot read length-prepended messages that are too big",
        ));
    }
    let mut value = vec![0u8; len];
    input.read_exact(&mut value).await?;

    Ok(value)
}
