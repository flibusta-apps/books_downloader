pub mod config;
pub mod views;
pub mod services;

use std::net::SocketAddr;
use tracing::info;

use crate::{views::get_router, config::CONFIG};


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let _guard = sentry::init(CONFIG.sentry_dsn.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router().await;

    info!("Start webserver...");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    info!("Webserver shutdown...")
}
