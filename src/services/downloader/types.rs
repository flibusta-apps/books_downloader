use reqwest::Response;
use std::pin::Pin;
use tempfile::SpooledTempFile;
use tokio::io::AsyncRead;

use futures::TryStreamExt;
use tokio_util::compat::FuturesAsyncReadCompatExt;

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
    it.bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        .into_async_read()
        .compat()
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
        match self.data {
            Data::Response(v) => Box::pin(get_response_async_read(v)),
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
