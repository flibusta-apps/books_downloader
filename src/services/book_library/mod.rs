pub mod types;

use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use std::time::Duration;

use crate::config;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build book_library HTTP client")
});

async fn _make_request<T>(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    params: Vec<(&str, String)>,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    T: DeserializeOwned,
{
    let response = client
        .get(url)
        .query(&params)
        .header("Authorization", api_key)
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
    let url = format!("{}/api/v1/sources", &config::CONFIG.book_library_url);
    _make_request(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![]).await
}

pub async fn get_book(
    book_id: u32,
) -> Result<types::BookWithRemote, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!(
        "{}/api/v1/books/{book_id}",
        &config::CONFIG.book_library_url
    );
    _make_request(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![]).await
}

pub async fn get_remote_book(
    source_id: u32,
    remote_id: u32,
) -> Result<types::BookWithRemote, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!(
        "{}/api/v1/books/remote/{source_id}/{remote_id}",
        &config::CONFIG.book_library_url
    );
    match _make_request::<types::Book>(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![])
        .await
    {
        Ok(v) => Ok(types::BookWithRemote::from_book(v, remote_id)),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stalled_server_times_out_quickly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                std::future::pending::<()>().await;
            }
        });

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(200))
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();

        let start = tokio::time::Instant::now();
        let result: Result<serde_json::Value, _> =
            _make_request(&client, &format!("http://{addr}/x"), "key", vec![]).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(2),
            "expected timeout to fire quickly, took {elapsed:?}"
        );
    }
}
