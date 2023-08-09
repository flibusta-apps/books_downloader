use reqwest::{Response, multipart::{Form, Part}, Body};
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::types::SpooledTempAsyncRead;

pub async fn convert_file(file: SpooledTempFile, file_type: String) -> Option<Response> {
    let client = reqwest::Client::new();

    let async_file = Body::wrap_stream(ReaderStream::new(SpooledTempAsyncRead::new(file)));
    let file_part = Part::stream(async_file).file_name("file");
    let form = Form::new()
        .text("format", file_type.clone())
        .part("file", file_part);

    let response = client
        .post(&config::CONFIG.converter_url)
        .multipart(form)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .send().await;

    let response = match response {
        Ok(v) => v,
        Err(_) => {
            return None
        },
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(_) => {
            return None
        },
    };

    Some(response)
}
