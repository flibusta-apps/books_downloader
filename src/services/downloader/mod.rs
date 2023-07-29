pub mod types;
pub mod utils;
pub mod zip;

use reqwest::Response;

use crate::config;

use self::types::{DownloadResult, Data, SpooledTempAsyncRead};
use self::utils::response_to_tempfile;
use self::zip::{unzip, zip};

use super::book_library::types::BookWithRemote;
use super::covert::convert_file;
use super::{book_library::get_remote_book, filename_getter::get_filename_by_book};

use futures::stream::FuturesUnordered;
use futures::StreamExt;

pub async fn download<'a>(
    book_id: &'a u32,
    book_file_type: &'a str,
    source_config: &'a config::SourceConfig,
) -> Option<(Response, bool)> {
    let basic_url = &source_config.url;
    let proxy = &source_config.proxy;

    let url = if book_file_type == "fb2" || book_file_type == "epub" || book_file_type == "mobi" {
        format!("{basic_url}/b/{book_id}/{book_file_type}")
    } else {
        format!("{basic_url}/b/{book_id}/download")
    };

    let client = match proxy {
        Some(v) => {
            let proxy_data = reqwest::Proxy::http(v);
            reqwest::Client::builder()
                .proxy(proxy_data.unwrap())
                .build()
                .unwrap()
        }
        None => reqwest::Client::new(),
    };

    let response = client.get(url).send().await;

    let response = match response {
        Ok(v) => v,
        Err(_) => return None,
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(_) => return None,
    };

    let headers = response.headers();
    let content_type = match headers.get("Content-Type") {
        Some(v) => v.to_str().unwrap(),
        None => "",
    };

    if book_file_type.to_lowercase() == "html" && content_type.contains("text/html") {
        return Some((response, false));
    }

    if content_type.contains("text/html")
    {
        return None;
    }

    let is_zip = content_type.contains("application/zip");

    Some((response, is_zip))
}

pub async fn download_chain<'a>(
    book: &'a BookWithRemote,
    file_type: &'a str,
    source_config: &'a config::SourceConfig,
    converting: bool
) -> Option<DownloadResult> {
    let final_need_zip = file_type == "fb2zip";

    let file_type_ = if converting {
        &book.file_type
    } else {
        file_type
    };

    let (mut response, is_zip) = match download(&book.remote_id, file_type_, source_config).await {
        Some(v) => v,
        None => return None,
    };

    if is_zip && book.file_type.to_lowercase() == "html" {
        let filename = get_filename_by_book(book, file_type, true, false);
        let filename_ascii = get_filename_by_book(book, file_type, true, true);
        let data_size: usize = response.headers().get("Content-Length").unwrap().to_str().unwrap().parse().unwrap();

        return Some(
            DownloadResult::new(
                Data::Response(response),
                filename,
                filename_ascii,
                data_size
            )
        );
    }

    if !is_zip && !final_need_zip && !converting {
        let filename = get_filename_by_book(book, &book.file_type, false, false);
        let filename_ascii = get_filename_by_book(book, file_type, false, true);
        let data_size: usize = response.headers().get("Content-Length").unwrap().to_str().unwrap().parse().unwrap();

        return Some(
            DownloadResult::new(
                Data::Response(response),
                filename,
                filename_ascii,
                data_size,
            )
        );
    };

    let (unziped_temp_file, data_size) = {
        let temp_file_to_unzip_result = response_to_tempfile(&mut response).await;
        let temp_file_to_unzip = match temp_file_to_unzip_result {
            Some(v) => v.0,
            None => return None,
        };

        match unzip(temp_file_to_unzip, "fb2") {
            Some(v) => v,
            None => return None,
        }
    };


    let (mut clean_file, data_size) = if converting {
        match convert_file(unziped_temp_file, file_type.to_string()).await {
            Some(mut response) => {
                match response_to_tempfile(&mut response).await {
                    Some(v) => v,
                    None => return None,
                }
            },
            None => return None,
        }
    } else {
        (unziped_temp_file, data_size)
    };

    if !final_need_zip {
        let t = SpooledTempAsyncRead::new(clean_file);
        let filename = get_filename_by_book(book, file_type, false, false);
        let filename_ascii = get_filename_by_book(book, file_type, false, true);

        return Some(
            DownloadResult::new(
                Data::SpooledTempAsyncRead(t),
                filename,
                filename_ascii,
                data_size
            )
        );
    };

    let t_file_type = if file_type == "fb2zip" { "fb2" } else { file_type };
    let filename = get_filename_by_book(book, t_file_type, false, false);
    match zip(&mut clean_file, filename.as_str()) {
        Some((t_file, data_size)) => {
            let t = SpooledTempAsyncRead::new(t_file);
            let filename = get_filename_by_book(book, file_type, true, false);
            let filename_ascii = get_filename_by_book(book, file_type, true, true);

            Some(
                DownloadResult::new(
                    Data::SpooledTempAsyncRead(t),
                    filename,
                    filename_ascii,
                    data_size
                )
            )
        },
        None => None,
    }
}

pub async fn start_download_futures(
    book: &BookWithRemote,
    file_type: &str,
) -> Option<DownloadResult> {
    let mut futures = FuturesUnordered::new();

    for source_config in &config::CONFIG.fl_sources {
        futures.push(download_chain(
            book,
            file_type,
            source_config,
            false
        ));

        if file_type == "epub" || file_type == "fb2" {
            futures.push(download_chain(
                book,
                file_type,
                source_config,
                true
            ))
        }
    }

    while let Some(result) = futures.next().await {
        if let Some(v) = result {
            return Some(v)
        }
    }

    None
}

pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
) -> Result<Option<DownloadResult>, Box<dyn std::error::Error + Send + Sync>> {
    let book = match get_remote_book(source_id, remote_id).await {
        Ok(v) => v,
        Err(err) => return Err(err),
    };

    match start_download_futures(&book, file_type).await {
        Some(v) => Ok(Some(v)),
        None => Ok(None),
    }
}
