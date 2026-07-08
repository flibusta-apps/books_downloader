use reqwest::Response;
use std::pin::Pin;
use tempfile::SpooledTempFile;
use tokio::io::{AsyncRead, AsyncReadExt};

use futures::TryStreamExt;
use tokio_util::io::StreamReader;

pub enum Data {
    Response(Response),
    SpooledTempAsyncRead(SpooledTempAsyncRead),
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
            Data::SpooledTempAsyncRead(v) => Box::pin(v),
        }
    }
}

pub struct SpooledTempAsyncRead {
    file: SpooledTempFile,
}

impl SpooledTempAsyncRead {
    pub fn new(file: SpooledTempFile) -> Self {
        Self { file }
    }
}

impl AsyncRead for SpooledTempAsyncRead {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let result = match std::io::Read::read(&mut self.get_mut().file, buf.initialize_unfilled())
        {
            Ok(v) => v,
            Err(err) => return std::task::Poll::Ready(Err(err)),
        };

        buf.set_filled(result);

        std::task::Poll::Ready(Ok(()))
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
}
