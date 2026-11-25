#![forbid(unsafe_code)]

use std::io::{Read, Write};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("I/O error on native messaging pipe: {0}")]
    Io(#[from] std::io::Error),

    #[error("unexpected EOF while reading native messaging header or payload")]
    UnexpectedEof,

    #[error("native messaging frame length {len} exceeds maximum {max_bytes} bytes")]
    FrameTooLarge { len: usize, max_bytes: usize },

    #[error("native messaging frame cannot have zero length")]
    EmptyFrame,

    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid chunk frame: {0}")]
    InvalidChunk(String),

    #[error("chunked payload exceeds reassembly cap of {max_bytes} bytes")]
    ReassemblyTooLarge { max_bytes: usize },

    #[error("too many concurrent chunked transfers (limit: {limit})")]
    TooManyTransfers { limit: usize },

    #[error("base64 decode error in chunked payload: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Reads one Native Messaging frame `[u32 little-endian length][UTF-8 JSON payload]` asynchronously.
///
/// Returns `Ok(None)` on clean EOF at a frame boundary. Rejects zero-length or oversized frames
/// before allocating payload buffers.
pub async fn read_frame_async<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, IpcError> {
    let mut header = [0u8; 4];
    let mut read_total = 0usize;
    while read_total < 4 {
        let n = reader.read(&mut header[read_total..]).await?;
        if n == 0 {
            if read_total == 0 {
                return Ok(None);
            }
            return Err(IpcError::UnexpectedEof);
        }
        read_total += n;
    }

    let len = u32::from_le_bytes(header) as usize;
    if len == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if len > max_bytes {
        return Err(IpcError::FrameTooLarge { len, max_bytes });
    }

    let mut payload = vec![0u8; len];
    match reader.read_exact(&mut payload).await {
        Ok(_) => Ok(Some(payload)),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(IpcError::UnexpectedEof),
        Err(err) => Err(IpcError::Io(err)),
    }
}

/// Writes one Native Messaging frame `[u32 little-endian length][UTF-8 JSON payload]` asynchronously and flushes.
pub async fn write_frame_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    max_bytes: usize,
) -> Result<(), IpcError> {
    let len = payload.len();
    if len == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if len > max_bytes {
        return Err(IpcError::FrameTooLarge { len, max_bytes });
    }
    let header = u32::try_from(len)
        .map_err(|_| IpcError::FrameTooLarge { len, max_bytes })?
        .to_le_bytes();

    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Synchronous 4-byte LE frame reader for dedicated blocking OS pipe threads (e.g. `stdin` on Windows).
pub fn read_frame_sync<R: Read>(reader: &mut R, max_bytes: usize) -> Result<Option<Vec<u8>>, IpcError> {
    let mut header = [0u8; 4];
    let mut read_total = 0usize;
    while read_total < 4 {
        let n = reader.read(&mut header[read_total..])?;
        if n == 0 {
            if read_total == 0 {
                return Ok(None);
            }
            return Err(IpcError::UnexpectedEof);
        }
        read_total += n;
    }

    let len = u32::from_le_bytes(header) as usize;
    if len == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if len > max_bytes {
        return Err(IpcError::FrameTooLarge { len, max_bytes });
    }

    let mut payload = vec![0u8; len];
    match reader.read_exact(&mut payload) {
        Ok(()) => Ok(Some(payload)),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(IpcError::UnexpectedEof),
        Err(err) => Err(IpcError::Io(err)),
    }
}

/// Synchronous 4-byte LE frame writer for dedicated blocking OS pipe threads (e.g. `stdout` on Windows).
pub fn write_frame_sync<W: Write>(
    writer: &mut W,
    payload: &[u8],
    max_bytes: usize,
) -> Result<(), IpcError> {
    let len = payload.len();
    if len == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if len > max_bytes {
        return Err(IpcError::FrameTooLarge { len, max_bytes });
    }
    let header = u32::try_from(len)
        .map_err(|_| IpcError::FrameTooLarge { len, max_bytes })?
        .to_le_bytes();

    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[tokio::test]
    async fn async_roundtrip_and_clean_eof() {
        let mut buf = Vec::new();
        let payload = br#"{"v":1,"kind":"event","event":"ping","payload":{}}"#;
        write_frame_async(&mut buf, payload, 1024).await.unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_frame_async(&mut cursor, 1024).await.unwrap().unwrap();
        assert_eq!(decoded, payload);
        assert!(read_frame_async(&mut cursor, 1024).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_truncated_header_and_oversize_frame() {
        let mut truncated = Cursor::new(vec![10u8, 0u8]);
        assert!(matches!(
            read_frame_async(&mut truncated, 1024).await,
            Err(IpcError::UnexpectedEof)
        ));

        let mut oversize = Vec::new();
        oversize.extend_from_slice(&(2048u32).to_le_bytes());
        oversize.extend_from_slice(&[b'a'; 16]);
        let mut cursor = Cursor::new(oversize);
        assert!(matches!(
            read_frame_async(&mut cursor, 1024).await,
            Err(IpcError::FrameTooLarge {
                len: 2048,
                max_bytes: 1024
            })
        ));
    }

    #[test]
    fn sync_roundtrip_rejects_zero_length() {
        let mut buf = Vec::new();
        write_frame_sync(&mut buf, b"{\"ok\":true}", 1024).unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(
            read_frame_sync(&mut cursor, 1024).unwrap().unwrap(),
            b"{\"ok\":true}"
        );

        let mut zero = Cursor::new(0u32.to_le_bytes().to_vec());
        assert!(matches!(
            read_frame_sync(&mut zero, 1024),
            Err(IpcError::EmptyFrame)
        ));
    }
}
