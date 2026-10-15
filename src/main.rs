use std::time::Duration;

use actix_web::middleware::{DefaultHeaders, Logger};
use actix_web::{web, App, HttpServer};

mod drm;
mod error;
mod handler;
mod hdhr;
mod m3u;
mod mpd;
mod ts;

/// Sent upstream by the proxy and by GPAC: some CDNs refuse other clients.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// The environment variable `name` parsed, or `default` when unset or invalid.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("HTTP client");
    let init_cache = handler::InitCache(
        moka::future::Cache::builder()
            .max_capacity(500)
            .time_to_live(Duration::from_secs(600))
            .build(),
    );

    HttpServer::new(move || {
        App::new()
            // For players in a browser. Plain GETs: no preflight to answer.
            .wrap(DefaultHeaders::new().add(("Access-Control-Allow-Origin", "*")))
            // The path carries keys and templates: log what it asks for instead.
            .wrap(
                Logger::new("%a \"%{target}xi\" %s %Dms")
                    .custom_request_replace("target", |req| handler::log_target(req.uri())),
            )
            .app_data(web::Data::new(client.clone()))
            .app_data(web::Data::new(init_cache.clone()))
            .default_service(web::get().to(handler::handle))
    })
    .bind(("0.0.0.0", 8888))?
    .run()
    .await
}
