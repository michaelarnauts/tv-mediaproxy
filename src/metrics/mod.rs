//! Lightweight request/traffic metrics using atomic counters.
//!
//! `AppMetrics` is created once and stored in actix-web `Data<AppMetrics>`.
//! Every handler increments the relevant counters; `GET /metrics` returns them as JSON.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use actix_web::{web, HttpResponse};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct AppMetrics {
    pub total_requests: AtomicU64,
    pub bytes_out: AtomicU64,

    /// Unix timestamp (seconds) when the process started.
    pub start_time: AtomicU64,
}

impl AppMetrics {
    pub fn new() -> Arc<Self> {
        let m = Arc::new(Self::default());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        m.start_time.store(now, Ordering::Relaxed);
        m
    }

    pub fn inc_request(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_bytes_out(&self, n: u64) {
        self.bytes_out.fetch_add(n, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// JSON response shape
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MetricsResponse {
    uptime_seconds: u64,
    total_requests: u64,
    bytes_out: u64,
    bytes_out_human: String,
}

fn human_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut val = b as f64;
    let mut i = 0;
    while val >= 1024.0 && i < UNITS.len() - 1 {
        val /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{val:.0} {}", UNITS[i])
    } else {
        format!("{val:.2} {}", UNITS[i])
    }
}

// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------

pub async fn metrics_handler(metrics: web::Data<Arc<AppMetrics>>) -> HttpResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let started = metrics.start_time.load(Ordering::Relaxed);
    let uptime = now.saturating_sub(started);
    let bytes_out = metrics.bytes_out.load(Ordering::Relaxed);

    HttpResponse::Ok().json(MetricsResponse {
        uptime_seconds: uptime,
        total_requests: metrics.total_requests.load(Ordering::Relaxed),
        bytes_out,
        bytes_out_human: human_bytes(bytes_out),
    })
}
