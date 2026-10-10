use actix_cors::Cors;
use actix_web::middleware::Logger;
use actix_web::{web, App, HttpServer};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod drm;
mod error;
mod handler;
mod hls;
mod m3u;
mod metrics;
mod mpd;
mod proxy;
mod ts;
mod utils;

use config::Config;
use handler::{InitCache, MpdCache};
use metrics::AppMetrics;
use proxy::stream::StreamManager;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Load config first so log_level from APP__LOG_LEVEL is available.
    let config = Config::from_env().expect("Failed to load configuration");

    // RUST_LOG takes precedence; fall back to config.log_level.
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| config.log_level.clone());

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(log_filter))
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_file(false)
                .with_line_number(false)
                .with_span_events(FmtSpan::NONE),
        )
        .try_init()
        .expect("Failed to initialize logging");

    // The password is the first path segment of every proxied URL; without one
    // the proxy would be open to anyone who can reach it.
    let password = &config.auth.api_password;
    if password.is_empty() || password.contains('/') {
        tracing::error!("Set API_PASSWORD (without slashes): it is the first segment of every URL");
        std::process::exit(1);
    }

    let stream_manager = StreamManager::new(config.proxy.clone());
    let server_config = Arc::new(config.clone());
    let app_metrics = AppMetrics::new();
    let init_cache = web::Data::new(InitCache(
        moka::future::Cache::builder()
            .max_capacity(500)
            .time_to_live(Duration::from_secs(config.mpd.init_cache_ttl.max(1)))
            .build(),
    ));

    // How fresh an entry must be depends on the request: see `fetch_mpd`.
    let mpd_cache = web::Data::new(MpdCache(
        moka::future::Cache::builder()
            .max_capacity(100)
            .time_to_live(handler::MPD_MAX_AGE_SEGMENT)
            .build(),
    ));

    tracing::info!(
        "Starting tv-mediaproxy on {}:{}",
        server_config.server.host,
        server_config.server.port
    );

    HttpServer::new(move || {
        App::new()
            .wrap(Cors::permissive())
            // The path carries the password and keys: log a redacted one.
            .wrap(
                Logger::new("%a \"%{path}xi\" %s %Dms")
                    .custom_request_replace("path", |req| handler::redacted_path(req.path())),
            )
            // No Compress: the proxy serves already-compressed media.
            .app_data(web::Data::new(stream_manager.clone()))
            .app_data(web::Data::new(Arc::clone(&server_config)))
            .app_data(web::Data::new(app_metrics.clone()))
            .app_data(init_cache.clone())
            .app_data(mpd_cache.clone())
            .route("/health", web::get().to(|| async { "OK" }))
            .route("/metrics", web::get().to(metrics::metrics_handler))
            .default_service(web::get().to(handler::proxy_handler))
    })
    .workers(config.server.workers)
    .bind((config.server.host.as_str(), config.server.port))?
    .run()
    .await
}
