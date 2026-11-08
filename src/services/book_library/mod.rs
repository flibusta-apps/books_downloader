pub mod error;
pub mod types;

use serde::de::DeserializeOwned;

use crate::config;

use super::http_client::CLIENT;
use error::BookLibraryError;

async fn _make_request<T>(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    params: Vec<(&str, String)>,
) -> Result<T, BookLibraryError>
where
    T: DeserializeOwned,
{
    let response = client
        .get(url)
        .query(&params)
        .header("Authorization", api_key)
        .send()
        .await
        .map_err(BookLibraryError::RequestFailed)?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(BookLibraryError::NotFound);
    }

    let response = response
        .error_for_status()
        .map_err(BookLibraryError::UpstreamError)?;

    response
        .json::<T>()
        .await
        .map_err(BookLibraryError::RequestFailed)
}

pub async fn get_book(book_id: u32) -> Result<types::BookWithRemote, BookLibraryError> {
    let url = config::join_url(
        &config::CONFIG.book_library_url,
        &format!("api/v1/books/{book_id}"),
    );
    _make_request(
        &CLIENT,
        url.as_str(),
        &config::CONFIG.book_library_api_key,
        vec![],
    )
    .await
}

pub async fn get_remote_book(
    source_id: u32,
    remote_id: u32,
) -> Result<types::BookWithRemote, BookLibraryError> {
    let url = config::join_url(
        &config::CONFIG.book_library_url,
        &format!("api/v1/books/remote/{source_id}/{remote_id}"),
    );
    let book = _make_request::<types::Book>(
        &CLIENT,
        url.as_str(),
        &config::CONFIG.book_library_api_key,
        vec![],
    )
    .await?;
    Ok(types::BookWithRemote::from_book(book, remote_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn query_encoding_and_authorization_reach_upstream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request
                .starts_with("GET /books?existing=1&search=a+%26+b&lang=ru&lang=en HTTP/1.1\r\n"));
            assert!(request
                .to_ascii_lowercase()
                .contains("\r\nauthorization: test-key\r\n"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: application/json\r\n\r\n{}").await.unwrap();
        });
        let result: serde_json::Value = _make_request(
            &reqwest::Client::new(),
            &format!("http://{addr}/books?existing=1"),
            "test-key",
            vec![
                ("search", "a & b".into()),
                ("lang", "ru".into()),
                ("lang", "en".into()),
            ],
        )
        .await
        .unwrap();
        assert_eq!(result, serde_json::json!({}));
        server.await.unwrap();
    }

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

    #[tokio::test]
    async fn not_found_status_maps_to_not_found_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let result: Result<serde_json::Value, _> =
            _make_request(&client, &format!("http://{addr}/x"), "key", vec![]).await;

        assert!(matches!(result, Err(BookLibraryError::NotFound)));
    }

    #[tokio::test]
    async fn server_error_status_maps_to_upstream_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let result: Result<serde_json::Value, _> =
            _make_request(&client, &format!("http://{addr}/x"), "key", vec![]).await;

        assert!(matches!(result, Err(BookLibraryError::UpstreamError(_))));
    }
}
