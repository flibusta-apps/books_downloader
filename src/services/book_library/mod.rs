pub mod types;

use serde::de::DeserializeOwned;

use crate::config;

async fn _make_request<T>(
    url: &str,
    params: Vec<(&str, String)>,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    T: DeserializeOwned,
{
    let client = reqwest::Client::new();

    let formated_url = format!("{}{}", &config::CONFIG.book_library_url, url);

    log::debug!("{}", formated_url);

    let response = client
        .get(formated_url)
        .query(&params)
        .header("Authorization", &config::CONFIG.book_library_api_key)
        .send()
        .await;

    let response = match response {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    match response.json::<T>().await {
        Ok(v) => Ok(v),
        Err(err) => Err(Box::new(err)),
    }
}

pub async fn get_sources() -> Result<types::Source, Box<dyn std::error::Error + Send + Sync>> {
    _make_request("/api/v1/sources", vec![]).await
}

pub async fn get_book(
    book_id: u32,
) -> Result<types::Book, Box<dyn std::error::Error + Send + Sync>> {
    _make_request(format!("/api/v1/books/{book_id}").as_str(), vec![]).await
}

pub async fn get_remote_book(
    source_id: u32,
    book_id: u32,
) -> Result<types::Book, Box<dyn std::error::Error + Send + Sync>> {
    _make_request(format!("/api/v1/books/remote/{source_id}/{book_id}").as_ref(), vec![]).await
}
