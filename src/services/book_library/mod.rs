pub mod types;

use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;

use crate::config;

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(reqwest::Client::new);

async fn _make_request<T>(
    url: &str,
    params: Vec<(&str, String)>,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    T: DeserializeOwned,
{
    let formatted_url = format!("{}{}", &config::CONFIG.book_library_url, url);

    let response = CLIENT
        .get(formatted_url)
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
) -> Result<types::BookWithRemote, Box<dyn std::error::Error + Send + Sync>> {
    _make_request(format!("/api/v1/books/{book_id}").as_str(), vec![]).await
}

pub async fn get_remote_book(
    source_id: u32,
    remote_id: u32,
) -> Result<types::BookWithRemote, Box<dyn std::error::Error + Send + Sync>> {
    match _make_request::<types::Book>(
        format!("/api/v1/books/remote/{source_id}/{remote_id}").as_ref(),
        vec![],
    )
    .await
    {
        Ok(v) => Ok(types::BookWithRemote::from_book(v, remote_id)),
        Err(err) => Err(err),
    }
}
