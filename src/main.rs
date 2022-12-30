#[macro_use]
extern crate lazy_static;

pub mod config;
pub mod views;
pub mod services;

use std::net::SocketAddr;
use axum::{Router, routing::get};
use axum_prometheus::PrometheusMetricLayer;
use views::{download, get_filename};


#[tokio::main]
async fn main() {
    env_logger::init();

    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app = Router::new()
        .route("/download/:source_id/:remote_id/:file_type", get(download))
        .route("/filename/:book_id/:file_type", get(get_filename))
        .route("/metrics", get(|| async move { metric_handle.render() }))
        .layer(prometheus_layer);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    log::info!("Start webserver...");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    log::info!("Webserver shutdown...")
}
