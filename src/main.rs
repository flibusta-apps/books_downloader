pub mod config;
pub mod services;
pub mod views;

use dotenv::dotenv;

use sentry::{integrations::debug_images::DebugImagesIntegration, types::Dsn, ClientOptions};
use std::{net::SocketAddr, str::FromStr};
use tracing::info;

use crate::views::get_router;

#[tokio::main]
async fn main() {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let options = ClientOptions {
        dsn: Some(Dsn::from_str(&config::CONFIG.sentry_dsn).unwrap()),
        default_integrations: false,
        ..Default::default()
    }
    .add_integration(DebugImagesIntegration::new());

    let _guard = sentry::init(options);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router().await;

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
    info!("Webserver shutdown...")
}
