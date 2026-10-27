use bytes::Bytes;
use reqwest::Response;
use std::io::Read;
use std::pin::Pin;
use tempfile::SpooledTempFile;
use tokio::io::{AsyncRead, AsyncReadExt};

use futures::TryStreamExt;
use tokio_util::io::StreamReader;

pub enum Data {
    Response(Response),
    SpooledTempFile(SpooledTempFile),
}

pub struct DownloadResult {
    pub data: Data,
    pub filename: String,
    pub filename_ascii: String,
    pub data_size: usize,
}

pub fn get_response_async_read(it: Response) -> impl AsyncRead {
    let stream = it.bytes_stream().map_err(std::io::Error::other);

    StreamReader::new(stream)
}

fn limit_async_read<R: AsyncRead>(read: R, limit: u64) -> impl AsyncRead {
    read.take(limit)
}

/// Streams a `SpooledTempFile` (sync `Read`) into an `AsyncRead` without blocking a
/// tokio worker thread: the blocking reads run on the blocking pool and are piped
/// across an in-memory duplex to the async side.
///
/// A read error is logged and propagated to the consumer as an `io::Error` (so the
/// HTTP body aborts instead of looking like a clean, truncated EOF).
pub fn spooled_temp_file_into_async_read(file: SpooledTempFile) -> Pin<Box<dyn AsyncRead + Send>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);

    drop(tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .blocking_send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        // Consumer went away; nothing more to do.
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    tracing::error!(error = %err, "failed to read spooled temp file while streaming");
                    let _ = tx.blocking_send(Err(err));
                    break;
                }
            }
        }
    }));

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Box::pin(StreamReader::new(stream))
}

impl DownloadResult {
    pub fn new(data: Data, filename: String, filename_ascii: String, data_size: usize) -> Self {
        Self {
            data,
            filename,
            filename_ascii,
            data_size,
        }
    }

    pub fn get_async_read(self) -> Pin<Box<dyn AsyncRead + Send>> {
        let data_size = self.data_size as u64;

        match self.data {
            Data::Response(v) => Box::pin(limit_async_read(get_response_async_read(v), data_size)),
            Data::SpooledTempFile(v) => spooled_temp_file_into_async_read(v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limit_async_read_truncates_to_declared_length() {
        let data: &[u8] = b"HELLO WORLD EXTRA BYTES";
        let mut limited = limit_async_read(data, 5);

        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf)
            .await
            .unwrap();

        assert_eq!(buf, b"HELLO");
    }

    #[tokio::test]
    async fn spooled_temp_file_streams_large_file_byte_identical() {
        use std::io::{Seek, Write};

        // Larger than the 5 MiB spool threshold, so the temp file rolls over to disk.
        let original: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

        let mut file = tempfile::spooled_tempfile(5 * 1024 * 1024);
        file.write_all(&original).unwrap();
        file.rewind().unwrap();

        let mut reader = spooled_temp_file_into_async_read(file);
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();

        assert_eq!(buf, original);
    }
}
