use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Fb2,
    Fb2Zip,
    Epub,
    Mobi,
    Html,
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Fb2 => "fb2",
            FileType::Fb2Zip => "fb2zip",
            FileType::Epub => "epub",
            FileType::Mobi => "mobi",
            FileType::Html => "html",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<FileType, serde_json::Error> {
        let json = serde_json::to_string(raw).unwrap();
        serde_json::from_str(&json)
    }

    #[test]
    fn accepts_all_allowlisted_values() {
        assert_eq!(parse("fb2").unwrap(), FileType::Fb2);
        assert_eq!(parse("fb2zip").unwrap(), FileType::Fb2Zip);
        assert_eq!(parse("epub").unwrap(), FileType::Epub);
        assert_eq!(parse("mobi").unwrap(), FileType::Mobi);
        assert_eq!(parse("html").unwrap(), FileType::Html);
    }

    #[test]
    fn rejects_path_traversal_payload() {
        // What "%2F" decodes to before it reaches our validation.
        assert!(parse("../admin").is_err());
    }

    #[test]
    fn rejects_query_injection_payload() {
        // What "%3F" decodes to before it reaches our validation.
        assert!(parse("epub?x=y").is_err());
    }

    #[test]
    fn rejects_case_variants() {
        assert!(parse("EPUB").is_err());
        assert!(parse("Fb2").is_err());
    }

    #[test]
    fn rejects_unknown_extension() {
        assert!(parse("pdf").is_err());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse("").is_err());
    }

    #[test]
    fn as_str_round_trips_through_deserialize() {
        for ft in [
            FileType::Fb2,
            FileType::Fb2Zip,
            FileType::Epub,
            FileType::Mobi,
            FileType::Html,
        ] {
            assert_eq!(parse(ft.as_str()).unwrap(), ft);
        }
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use axum::{extract::Path, http::StatusCode, routing::get, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::net::TcpListener;

    async fn spawn_download_route(hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/download/{source_id}/{remote_id}/{file_type}",
            get(move |Path((_, _, _)): Path<(u32, u32, FileType)>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn spawn_filename_route(hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/filename/{book_id}/{file_type}",
            get(move |Path((_, _)): Path<(u32, FileType)>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn download_route_rejects_path_traversal_before_reaching_handler() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_download_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/download/1/1/..%2Fadmin"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "handler must never run for an invalid file_type"
        );
    }

    #[tokio::test]
    async fn download_route_rejects_query_injection_before_reaching_handler() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_download_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/download/1/1/epub%3Fx=y"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn download_route_accepts_allowlisted_file_type() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_download_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/download/1/1/fb2"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn filename_route_rejects_path_traversal_before_reaching_handler() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_filename_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/filename/1/..%2Fadmin"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
