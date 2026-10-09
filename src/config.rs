use config::{Map, Value};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::warn;
use url::Url;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub workers: usize,
    /// Public URL path prefix for generated URLs behind a reverse proxy.
    ///
    /// Empty string means no prefix. Non-empty values are normalized to start
    /// with `/` and not end with `/`, e.g. `/tv` or `/api/v1`.
    /// Values containing whitespace, control characters, or URL delimiters are
    /// rejected during configuration loading.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct ProxyRouteConfig {
    #[serde(default)]
    pub proxy: bool,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,
}

fn default_verify_ssl() -> bool {
    true
}

// ── ProxyConfig defaults ────────────────────────────────────────────────────
//
// These apply to omitted TOML keys so older config files keep working.
fn default_request_timeout_factor() -> u64 {
    8
}
fn default_max_concurrent_per_host() -> usize {
    10
}
fn default_pool_idle_timeout() -> u64 {
    90
}
fn default_pool_max_idle_per_host() -> usize {
    100
}
fn default_body_read_timeout() -> u64 {
    60
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProxyConfig {
    /// TCP handshake timeout (seconds) for a new upstream connection.
    pub connect_timeout: u64,

    /// Whether reqwest should follow 3xx redirects.
    pub follow_redirects: bool,

    /// Optional default upstream HTTP/HTTPS/SOCKS proxy URL.
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// If true, route all upstream traffic through `proxy_url`.
    #[serde(default)]
    pub all_proxy: bool,

    /// Per-pattern transport overrides (proxy, SSL verification).
    #[serde(default)]
    pub transport_routes: HashMap<String, ProxyRouteConfig>,

    // ── Tunables (all have sensible defaults) ─────────────────────────────
    /// Multiplier applied to `connect_timeout` to derive the full request
    /// timeout (covers pool-wait + connect + TLS + response headers).
    ///
    /// Default: `8` → request timeout = connect_timeout × 8.
    #[serde(default = "default_request_timeout_factor")]
    pub request_timeout_factor: u64,

    /// Maximum concurrent in-flight upstream requests per origin host.
    /// Matches aiohttp's `limit_per_host=10` default; set to 0 to disable.
    /// Capping here forces HTTP/1.1 keep-alive reuse for bursty traffic
    /// and dramatically reduces per-request latency.
    ///
    /// Default: `10`.
    #[serde(default = "default_max_concurrent_per_host")]
    pub max_concurrent_per_host: usize,

    /// Seconds an idle upstream connection is kept in the pool before eviction.
    ///
    /// Default: `90`.
    #[serde(default = "default_pool_idle_timeout")]
    pub pool_idle_timeout: u64,

    /// Maximum idle upstream connections retained per host per worker.
    ///
    /// Default: `100`.
    #[serde(default = "default_pool_max_idle_per_host")]
    pub pool_max_idle_per_host: usize,

    /// Timeout (seconds) applied to fully reading a small in-memory body
    /// via `fetch_bytes()` (used for manifests, playlists, EPG data).
    ///
    /// Default: `60`.
    #[serde(default = "default_body_read_timeout")]
    pub body_read_timeout: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub api_password: String,
}

// ---------------------------------------------------------------------------
// MPD / DASH config
// ---------------------------------------------------------------------------

fn default_mpd_init_cache_ttl() -> u64 {
    600
}

#[derive(Debug, Deserialize, Clone)]
pub struct MpdConfig {
    /// TTL in seconds for cached init segments. Every media segment needs its
    /// representation's init segment to be decrypted.
    #[serde(default = "default_mpd_init_cache_ttl")]
    pub init_cache_ttl: u64,
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            init_cache_ttl: default_mpd_init_cache_ttl(),
        }
    }
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub proxy: ProxyConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub mpd: MpdConfig,
    /// Log filter directive (e.g. "debug", "info", "tv_mediaproxy=debug,info").
    /// Overrides the RUST_LOG env var when set.
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String {
    "info,actix_http::h1=off".to_string()
}

// ---------------------------------------------------------------------------
// Proxy routing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ProxyRoute {
    pub pattern: Regex,
    pub config: ProxyRouteConfig,
}

#[derive(Debug, Clone)]
pub struct ProxyRouter {
    default_proxy: Option<String>,
    all_proxy: bool,
    routes: Vec<ProxyRoute>,
}

impl ProxyRouter {
    pub fn new(
        default_proxy: Option<String>,
        all_proxy: bool,
        routes_config: HashMap<String, ProxyRouteConfig>,
    ) -> Self {
        let mut routes = Vec::new();

        for (pattern, config) in routes_config {
            let pattern = pattern
                .replace(".", "\\.")
                .replace("*", "[^/]*")
                .replace("all://", "(http|https)://");

            match Regex::new(&format!("^{}", pattern)) {
                Ok(regex) => {
                    routes.push(ProxyRoute {
                        pattern: regex,
                        config,
                    });
                }
                Err(e) => {
                    tracing::error!("Invalid route pattern '{}': {}", pattern, e);
                }
            }
        }

        // Sort routes by specificity (fewer wildcards = more specific = higher priority)
        routes.sort_by(|a, b| {
            let a_wildcards = a.pattern.as_str().matches("[^/]*").count();
            let b_wildcards = b.pattern.as_str().matches("[^/]*").count();
            b_wildcards.cmp(&a_wildcards)
        });

        Self {
            default_proxy,
            all_proxy,
            routes,
        }
    }

    pub fn from_config(config: &ProxyConfig) -> Self {
        Self::new(
            config.proxy_url.clone(),
            config.all_proxy,
            config.transport_routes.clone(),
        )
    }

    pub fn get_proxy_config(&self, url: &str) -> Option<ProxyRouteConfig> {
        match Url::parse(url) {
            Ok(parsed_url) => {
                let url_str = parsed_url.as_str();

                for route in &self.routes {
                    if route.pattern.is_match(url_str) {
                        tracing::debug!("Matched route pattern: {}", route.pattern.as_str());
                        return Some(route.config.clone());
                    }
                }

                if self.all_proxy {
                    return Some(ProxyRouteConfig {
                        proxy: true,
                        proxy_url: self.default_proxy.clone(),
                        verify_ssl: true,
                    });
                }
            }
            Err(e) => {
                tracing::error!("Failed to parse URL '{}': {}", url, e);
            }
        }

        None
    }

    pub fn default_proxy(&self) -> &Option<String> {
        &self.default_proxy
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

impl Config {
    pub fn from_env() -> Result<Self, config::ConfigError> {
        let mut builder = config::Config::builder()
            // Server
            .set_default("server.host", "127.0.0.1")?
            .set_default("server.port", 8888)?
            .set_default("server.workers", 4)?
            .set_default("server.path", "")?
            // Proxy
            .set_default("proxy.connect_timeout", 30)?
            .set_default("proxy.follow_redirects", true)?
            .set_default("proxy.all_proxy", false)?
            .set_default("proxy.transport_routes", HashMap::<String, Value>::new())?
            // Auth
            .set_default("auth.api_password", "")?
            // MPD
            .set_default("mpd.init_cache_ttl", 600)?;

        if let Ok(config_path) = std::env::var("CONFIG_PATH") {
            let path = Path::new(&config_path);
            if path.exists() {
                builder = builder.add_source(config::File::with_name(&config_path));
            } else {
                warn!("Config file not found at {}", config_path);
            }
        }

        // Python mediaflow-proxy compatibility: honour flat env vars when the
        // structured APP__* equivalents are absent.  This lets users swap the
        // Docker image without changing their environment.
        let compat_pairs: &[(&str, &str)] = &[
            ("API_PASSWORD", "auth.api_password"),
            ("HOST", "server.host"),
            ("PORT", "server.port"),
            ("WORKERS", "server.workers"),
            ("PROXY_URL", "proxy.proxy_url"),
            ("ALL_PROXY", "proxy.all_proxy"),
        ];
        for (env_var, config_key) in compat_pairs {
            if let Ok(val) = std::env::var(env_var) {
                // Only apply when the APP__* equivalent is absent so the
                // structured form always wins.
                let app_key = format!("APP__{}", config_key.to_uppercase().replace('.', "__"));
                if std::env::var(&app_key).is_err() {
                    builder = builder.set_override(config_key, val)?;
                }
            }
        }

        builder = builder.add_source(
            config::Environment::with_prefix("APP")
                .separator("__")
                .try_parsing(true),
        );

        if let Ok(routes_json) = std::env::var("APP__PROXY__TRANSPORT_ROUTES") {
            match serde_json::from_str::<HashMap<String, ProxyRouteConfig>>(&routes_json) {
                Ok(routes) => {
                    let routes_map = routes
                        .into_iter()
                        .map(|(k, v)| {
                            let mut inner_map = Map::new();
                            inner_map.insert("proxy".into(), Value::from(v.proxy));
                            if let Some(url) = v.proxy_url {
                                inner_map.insert("proxy_url".into(), Value::from(url));
                            }
                            inner_map.insert("verify_ssl".into(), Value::from(v.verify_ssl));
                            (k, Value::from(inner_map))
                        })
                        .collect::<Map<String, Value>>();

                    builder =
                        builder.set_override("proxy.transport_routes", Value::from(routes_map))?;
                }
                Err(e) => {
                    return Err(config::ConfigError::Message(format!(
                        "Failed to parse TRANSPORT_ROUTES: {}",
                        e
                    )));
                }
            }
        }

        let config = builder.build()?;
        let mut config: Self = config.try_deserialize()?;
        config.server.path = normalize_server_path(&config.server.path)?;
        Ok(config)
    }
}

fn normalize_server_path(path: &str) -> Result<String, config::ConfigError> {
    if path
        .bytes()
        .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        || path.contains(['?', '#', '\\'])
    {
        return Err(config::ConfigError::Message(
            "server.path must be empty or a URL path prefix like /tv".to_string(),
        ));
    }

    let collapsed = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if collapsed.is_empty() {
        return Ok(String::new());
    }

    let normalized = format!("/{}", collapsed.join("/"));
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::normalize_server_path;

    #[test]
    fn normalize_server_path_collapses_repeated_slashes() {
        assert_eq!(
            normalize_server_path("/foo//bar///baz").unwrap(),
            "/foo/bar/baz"
        );
        assert_eq!(normalize_server_path("////").unwrap(), "");
    }

    #[test]
    fn normalize_server_path_rejects_whitespace() {
        assert!(normalize_server_path(" /mediaflow ").is_err());
    }
}
