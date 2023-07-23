pub mod config;
pub mod views;
pub mod services;

use std::net::SocketAddr;
use axum::{Router, routing::get, middleware::{self, Next}, http::{self, Request}, response::Response};
use axum_prometheus::PrometheusMetricLayer;
use config::CONFIG;
use reqwest::StatusCode;
use views::{download, get_filename};
use tracing::info;
use tower_http::trace::{TraceLayer, self};
use tracing::Level;


async fn auth<B>(req: Request<B>, next: Next<B>) -> Result<Response, StatusCode> {
    let auth_header = req.headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if auth_header != CONFIG.api_key {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}


async fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route("/download/:source_id/:remote_id/:file_type", get(download))
        .route("/filename/:book_id/:file_type", get(get_filename))
        .layer(middleware::from_fn(auth))
        .layer(prometheus_layer);

    let metric_router = Router::new()
        .route("/metrics", get(|| async move { metric_handle.render() }));

    Router::new()
        .nest("/", app_router)
        .nest("/", metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new()
                    .level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new()
                    .level(Level::INFO)),
        )
}


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router().await;

    info!("Start webserver...");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    info!("Webserver shutdown...")
}
