//! The proxy route: `GET /{password}/{kind}/[{kid}:{key}[,…]/][t={templates}/]{upstream URL}`.
//!
//! The upstream URL is pasted as-is after the prefix (`https://…`, query
//! included). `kind` says what it is:
//!
//! - `m3u`: a playlist; ClearKey MPD entries are routed through `mpd`,
//! - `m3u-ts`: the same, routing ClearKey and unprotected MPD entries
//!   through `ts`,
//! - `mpd`: a manifest, rewritten for playback through this proxy
//!   (see `mpd::rewrite`),
//! - `ts`: a manifest played into one continuous MPEG-TS stream by GPAC
//!   (see `ts`); the keys are optional,
//! - `hls`: a manifest turned into an HLS master playlist (see `hls`); the
//!   keys are optional. Its media playlists and segments are addressed as
//!   `hls/[{keys}/]m={base64url(manifest URL)}/{rendition}/{file}`,
//! - `m3u-hls`: like `m3u`, routing ClearKey and unprotected MPD entries
//!   through `hls`,
//! - `hdhr`: an HDHomeRun tuner whose channels are the playlist's `ts`
//!   entries (see `hdhr`); the client appends `/discover.json` and the like,
//! - `seg`: an init or media segment of protected content. Both resolve from
//!   the same `BaseURL`, so the URL tells them apart: one that matches the
//!   media template is a media segment and gets decrypted with its init
//!   segment; anything else is the init segment and loses its protection.
//!
//! `t=` carries the base64url absolute initialization and media templates; the
//! MPD rewrite puts it in the `BaseURL` of protected content, so the segment
//! URLs a player builds lead back to their init segment (`mpd::template`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::{web, HttpRequest, HttpResponse};
use base64::Engine;
use futures::TryStreamExt;
use reqwest::header::HeaderMap;

use crate::hdhr;
use crate::hls::{master_playlist, media_playlist, resolve, HlsParams};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    m3u::{rewrite_m3u, Output},
    metrics::AppMetrics,
    mpd::rewrite::{rewrite_mpd, RewriteParams},
    mpd::template::init_for_segment,
    proxy::stream::StreamManager,
    utils::url::public_proxy_base_url,
};

/// Raw upstream init segments, keyed by URL.
#[derive(Clone)]
pub struct InitCache(pub moka::future::Cache<String, bytes::Bytes>);

#[derive(Debug, PartialEq)]
enum Kind {
    Hdhr,
    Hls,
    M3u,
    M3uHls,
    M3uTs,
    Mpd,
    Seg,
    Ts,
}

#[derive(Debug, PartialEq)]
struct Target {
    kind: Kind,
    /// `kid:key[,kid:key]`, hex.
    key: Option<String>,
    /// Absolute (initialization, media) templates of protected content.
    templates: Option<(String, String)>,
    /// Below `m=`: `{rendition}/{file}` of an HLS media playlist or segment.
    hls_path: Option<String>,
    url: String,
}

pub async fn proxy_handler(
    req: HttpRequest,
    stream_manager: web::Data<StreamManager>,
    config: web::Data<Arc<Config>>,
    metrics: web::Data<Arc<AppMetrics>>,
    init_cache: web::Data<InitCache>,
    mpd_cache: web::Data<MpdCache>,
) -> AppResult<HttpResponse> {
    metrics.inc_request();
    let password = &config.auth.api_password;
    let target = parse_target(req.uri().path(), req.query_string(), password)?;
    let prefix = format!(
        "{}/{password}",
        public_proxy_base_url(&req, &config.server.path)
    );

    let (body, content_type) = match target.kind {
        Kind::Ts => {
            // Start `start_buffer` seconds before the newest published
            // segment. Origins publish late by different amounts, so this is
            // measured from the manifest, not the clock.
            let manifest = fetch_mpd(
                &stream_manager,
                &mpd_cache,
                &target.url,
                MPD_MAX_AGE_PLAYLIST,
            )
            .await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64());
            // GPAC fetches a segment only once the clock has passed its end
            // (its DASH availability), so segments listed ahead of the clock
            // do not count.
            let behind = crate::hls::live_edge(&manifest.mpd, &manifest.url)
                .map_or(0.0, |edge| (now - edge).max(0.0));
            let delay_ms = ((behind + config.ts.start_buffer as f64) * 1000.0) as i64;
            tracing::info!(
                "ts: newest segment {behind:.1}s behind live, starting {:.1}s behind",
                delay_ms as f64 / 1000.0
            );
            // GPAC adds its spd to the manifest's suggestedPresentationDelay.
            let suggested_ms = manifest
                .mpd
                .suggestedPresentationDelay
                .map_or(0, |d| d.as_millis() as i64);
            let stream = crate::ts::play(
                &config.ts.gpac,
                &target.url,
                target.key.as_deref(),
                delay_ms - suggested_ms,
            )
            .await?
            .inspect_ok(move |chunk| metrics.add_bytes_out(chunk.len() as u64));
            return Ok(HttpResponse::Ok()
                .content_type("video/mp2t")
                .insert_header(("cache-control", "no-cache"))
                .streaming(stream));
        }
        Kind::Hls => {
            // Playlists need the newest segments; a segment only needs its
            // period, which an older copy has unless the period is new.
            let is_segment = target
                .hls_path
                .as_deref()
                .is_some_and(|p| !p.ends_with("/index.m3u8"));
            let max_age = if is_segment {
                MPD_MAX_AGE_SEGMENT
            } else {
                MPD_MAX_AGE_PLAYLIST
            };
            let mut manifest = fetch_mpd(&stream_manager, &mpd_cache, &target.url, max_age).await?;
            let Some(path) = &target.hls_path else {
                let (mpd, manifest_url) = (&manifest.mpd, manifest.url.as_str());
                let params = HlsParams {
                    source_url: &target.url,
                    prefix: &prefix,
                    key: target.key.as_deref(),
                };
                let playlist = master_playlist(mpd, &params)
                    .map_err(|e| AppError::Mpd(format!("{e} in MPD from {manifest_url}")))?;
                return Ok(playlist_response(playlist));
            };
            let (rendition, file) = path
                .split_once('/')
                .ok_or_else(|| AppError::BadRequest("Bad HLS path".into()))?;
            if file == "index.m3u8" {
                let playlist = media_playlist(&manifest.mpd, rendition, &manifest.url)
                    .map_err(|e| AppError::Mpd(format!("{e} in MPD from {}", manifest.url)))?;
                return Ok(playlist_response(playlist));
            }
            let mut resolved = resolve(&manifest.mpd, rendition, file, &manifest.url);
            if resolved.is_err() && manifest.fetched.elapsed() > MPD_MAX_AGE_PLAYLIST {
                // A period newer than the cached manifest.
                manifest =
                    fetch_mpd(&stream_manager, &mpd_cache, &target.url, Duration::ZERO).await?;
                resolved = resolve(&manifest.mpd, rendition, file, &manifest.url);
            }
            let resource = resolved.map_err(AppError::NotFound)?;
            let keys = target.key.as_deref().filter(|_| resource.protected);
            let body = match (resource.init_url, keys) {
                (None, Some(keys)) => {
                    let init = fetch_init(&stream_manager, &init_cache, resource.url).await?;
                    crate::drm::cenc::process_drm_init_segment(&init, keys)
                        .map_err(AppError::Drm)?
                        .into()
                }
                (None, None) => fetch_init(&stream_manager, &init_cache, resource.url).await?,
                (Some(init_url), Some(keys)) => {
                    let (init, segment) = futures::try_join!(
                        fetch_init(&stream_manager, &init_cache, init_url),
                        stream_manager.fetch_bytes(resource.url, HeaderMap::new()),
                    )?;
                    crate::drm::cenc::decrypt_segment(&init, &segment, keys, false)
                        .map_err(AppError::Drm)?
                        .into()
                }
                (Some(_), None) => {
                    stream_manager
                        .fetch_bytes(resource.url, HeaderMap::new())
                        .await?
                }
            };
            (body, "video/mp4")
        }
        Kind::Hdhr => {
            let (playlist, file) = hdhr::split(&target.url);
            let base = format!("{prefix}/hdhr/{playlist}");
            let id = hdhr::device_id(playlist);
            match file {
                hdhr::File::Discover => (
                    hdhr::discover(&base, &id, config.hdhr.tuners).into(),
                    "application/json",
                ),
                hdhr::File::LineupStatus => (hdhr::lineup_status().into(), "application/json"),
                hdhr::File::DeviceXml => (hdhr::device_xml(&base, &id).into(), "application/xml"),
                hdhr::File::Lineup => {
                    let m3u = stream_manager
                        .fetch_bytes(playlist.to_string(), HeaderMap::new())
                        .await?;
                    let routed = rewrite_m3u(&String::from_utf8_lossy(&m3u), &prefix, Output::Ts);
                    let channels = hdhr::channels(&routed, &format!("{prefix}/ts/"));
                    (hdhr::lineup(&channels).into(), "application/json")
                }
            }
        }
        Kind::M3u | Kind::M3uHls | Kind::M3uTs => {
            let output = match target.kind {
                Kind::M3u => Output::Mpd,
                Kind::M3uHls => Output::Hls,
                _ => Output::Ts,
            };
            let playlist = stream_manager
                .fetch_bytes(target.url.clone(), HeaderMap::new())
                .await?;
            let rewritten = rewrite_m3u(&String::from_utf8_lossy(&playlist), &prefix, output);
            (
                bytes::Bytes::from(rewritten),
                "audio/x-mpegurl; charset=utf-8",
            )
        }
        Kind::Mpd => {
            let fetched = stream_manager
                .fetch(target.url.clone(), HeaderMap::new())
                .await?;
            let rewritten = rewrite_mpd(
                &fetched.body,
                &RewriteParams {
                    manifest_url: &fetched.url,
                    prefix: &prefix,
                    key: target.key.as_deref(),
                },
            )
            .map_err(|e| {
                AppError::Mpd(format!("Failed to rewrite MPD from {}: {e}", fetched.url))
            })?;
            (bytes::Bytes::from(rewritten), "application/dash+xml")
        }
        Kind::Seg => {
            let keys = target.key.as_deref().unwrap_or_default();
            let (init_template, media_template) = target.templates.as_ref().ok_or_else(|| {
                AppError::BadRequest("seg needs t= (the segment templates)".into())
            })?;
            let body = match init_for_segment(init_template, media_template, &target.url) {
                Some(init_url) => {
                    let (init, segment) = futures::try_join!(
                        fetch_init(&stream_manager, &init_cache, init_url),
                        stream_manager.fetch_bytes(target.url.clone(), HeaderMap::new()),
                    )?;
                    crate::drm::cenc::decrypt_segment(&init, &segment, keys, false)
                }
                None => {
                    let init = fetch_init(&stream_manager, &init_cache, target.url.clone()).await?;
                    crate::drm::cenc::process_drm_init_segment(&init, keys)
                }
            }
            .map_err(AppError::Drm)?;
            (bytes::Bytes::from(body), "video/mp4")
        }
    };

    metrics.add_bytes_out(body.len() as u64);
    Ok(HttpResponse::Ok()
        .content_type(content_type)
        .insert_header(("cache-control", "no-cache"))
        .body(body))
}

/// How old a cached manifest may be for a playlist, and for a segment.
const MPD_MAX_AGE_PLAYLIST: Duration = Duration::from_secs(1);
pub const MPD_MAX_AGE_SEGMENT: Duration = Duration::from_secs(300);

/// A parsed manifest, its URL after redirects and when it was fetched.
pub struct Manifest {
    mpd: dash_mpd_core::MPD,
    url: String,
    fetched: Instant,
}

/// Manifests by URL: an HLS player fetches several playlists and many
/// segments of one manifest.
#[derive(Clone)]
pub struct MpdCache(pub moka::future::Cache<String, Arc<Manifest>>);

/// The manifest at `url`, from the cache unless older than `max_age`.
async fn fetch_mpd(
    sm: &StreamManager,
    cache: &MpdCache,
    url: &str,
    max_age: Duration,
) -> AppResult<Arc<Manifest>> {
    if let Some(manifest) = cache.0.get(url).await {
        if manifest.fetched.elapsed() <= max_age {
            return Ok(manifest);
        }
        cache.0.invalidate(url).await;
    }
    cache
        .0
        .try_get_with(url.to_string(), async {
            let fetched = sm.fetch(url.to_string(), HeaderMap::new()).await?;
            let mpd =
                dash_mpd_core::parse(&String::from_utf8_lossy(&fetched.body)).map_err(|e| {
                    AppError::Mpd(format!("Failed to parse MPD from {}: {e}", fetched.url))
                })?;
            Ok::<_, AppError>(Arc::new(Manifest {
                mpd,
                url: fetched.url,
                fetched: Instant::now(),
            }))
        })
        .await
        .map_err(|e| match e.as_ref() {
            AppError::NotFound(m) => AppError::NotFound(m.clone()),
            other => AppError::Upstream(other.to_string()),
        })
}

fn playlist_response(playlist: String) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(("cache-control", "no-cache"))
        .body(playlist)
}

/// An init segment, from the cache when its representation was seen before.
async fn fetch_init(sm: &StreamManager, cache: &InitCache, url: String) -> AppResult<bytes::Bytes> {
    cache
        .0
        .try_get_with(url.clone(), sm.fetch_bytes(url, HeaderMap::new()))
        .await
        .map_err(|e| match e.as_ref() {
            AppError::NotFound(m) => AppError::NotFound(m.clone()),
            other => AppError::Upstream(other.to_string()),
        })
}

/// Split the request path into password, kind, options and upstream URL.
fn parse_target(path: &str, query: &str, password: &str) -> AppResult<Target> {
    let usage = || {
        AppError::BadRequest(
            "Expected /{password}/{m3u|m3u-hls|m3u-ts|mpd|hls|seg|ts|hdhr}/…/https://…".into(),
        )
    };
    let path = path.strip_prefix('/').unwrap_or(path);
    let (given, rest) = path.split_once('/').unwrap_or((path, ""));
    if given != password {
        return Err(AppError::Auth("Invalid password".into()));
    }
    let (kind, mut rest) = rest.split_once('/').ok_or_else(usage)?;
    let kind = match kind {
        "hdhr" => Kind::Hdhr,
        "hls" => Kind::Hls,
        "m3u" => Kind::M3u,
        "m3u-hls" => Kind::M3uHls,
        "m3u-ts" => Kind::M3uTs,
        "mpd" => Kind::Mpd,
        "seg" => Kind::Seg,
        "ts" => Kind::Ts,
        _ => return Err(usage()),
    };

    let mut target = Target {
        kind,
        key: None,
        templates: None,
        hls_path: None,
        url: String::new(),
    };
    while !is_upstream_url(rest) {
        let (segment, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let segment = urlencoding::decode(segment)
            .map_err(|_| AppError::BadRequest("Bad path segment".into()))?;
        if is_key_list(&segment) {
            target.key = Some(segment.into_owned());
        } else if let Some(encoded) = segment.strip_prefix("t=") {
            let templates = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .and_then(|s| {
                    s.split_once('\n')
                        .map(|(i, m)| (i.to_string(), m.to_string()))
                })
                .ok_or_else(|| AppError::BadRequest("Bad t= segment".into()))?;
            target.templates = Some(templates);
        } else if let Some(encoded) = segment.strip_prefix("m=") {
            // The upstream URL, encoded: clients cannot rewrite its query.
            // Below it, HLS names its playlists and segments.
            target.url = decode_upstream(encoded)
                .ok_or_else(|| AppError::BadRequest("Bad m= segment".into()))?;
            if !tail.is_empty() {
                if target.kind != Kind::Hls {
                    return Err(AppError::BadRequest("Nothing may follow m=".into()));
                }
                target.hls_path = Some(tail.to_string());
            }
            return Ok(target);
        } else {
            return Err(AppError::BadRequest(format!(
                "Unknown path segment {segment:?}"
            )));
        }
        rest = tail;
    }
    if target.kind == Kind::Seg && target.key.is_none() {
        return Err(AppError::BadRequest("seg needs the keys".into()));
    }

    // Accept `https:/host` too: some clients and proxies collapse `//`.
    let (scheme, after) = rest.split_once(':').unwrap_or(("https", rest));
    target.url = format!("{scheme}://{}", after.trim_start_matches('/'));
    if !query.is_empty() {
        target.url.push('?');
        target.url.push_str(query);
    }
    Ok(target)
}

/// The request path for logging: password, keys and templates hidden, kind
/// and upstream URL kept.
pub fn redacted_path(path: &str) -> String {
    let mut segments = path.trim_start_matches('/').splitn(3, '/');
    let (_, kind, mut rest) = (
        segments.next(),
        segments.next().unwrap_or_default(),
        segments.next().unwrap_or_default(),
    );
    let mut out = format!("/…/{kind}");
    while !rest.is_empty() && !is_upstream_url(rest) {
        let (segment, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if let Some(encoded) = segment.strip_prefix("m=") {
            out.push_str("/m=");
            out.push_str(decode_upstream(encoded).as_deref().unwrap_or("?"));
            if !tail.is_empty() {
                out.push_str(" /");
                out.push_str(tail);
            }
            return out;
        }
        out.push_str(if segment.starts_with("t=") {
            "/t=…"
        } else {
            "/…"
        });
        rest = tail;
    }
    if !rest.is_empty() {
        out.push('/');
        out.push_str(rest);
    }
    out
}

/// The upstream URL of an `m=` segment.
fn decode_upstream(encoded: &str) -> Option<String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|u| is_upstream_url(u))
}

/// `m=` followed by `url`, base64url: an upstream URL in a path, so that
/// clients that normalise query strings cannot change what reaches it.
pub fn encode_upstream(url: &str) -> String {
    format!(
        "m={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url)
    )
}

/// Whether the rest of a path is the upstream URL. `https:/` with a single
/// slash counts: some clients and proxies collapse `//`.
fn is_upstream_url(rest: &str) -> bool {
    rest.starts_with("http:/") || rest.starts_with("https:/")
}

/// `kid:key[,kid:key]` with 32-digit hex values.
pub fn is_key_list(value: &str) -> bool {
    let is_hex = |s: &str| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit());
    value.split(',').all(|pair| {
        pair.trim()
            .split_once(':')
            .is_some_and(|(kid, key)| is_hex(kid) && is_hex(key))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "fa99f6cb1eb40f944b8b82507cc9ed6f:8e3265a443b9e30f47846df6ea3af11a";

    #[test]
    fn parses_playlist_url() {
        let t = parse_target("/pw/m3u/https://tv.example/iptv.m3u", "", "pw").unwrap();
        assert_eq!(t.kind, Kind::M3u);
        assert_eq!(t.url, "https://tv.example/iptv.m3u");
        assert_eq!(t.key, None);
    }

    #[test]
    fn parses_segment_with_key_templates_query_and_collapsed_slashes() {
        let templates = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://cdn/a/i-$RepresentationID$.mp4\nhttps://cdn/a/s-$Time$.m4s");
        let path = format!("/pw/seg/{KEY}/t={templates}/https:/cdn/a/s-1.m4s");
        let t = parse_target(&path, "m=1", "pw").unwrap();
        assert_eq!(t.kind, Kind::Seg);
        assert_eq!(t.url, "https://cdn/a/s-1.m4s?m=1");
        assert_eq!(t.key.as_deref(), Some(KEY));
        assert_eq!(
            t.templates,
            Some((
                "https://cdn/a/i-$RepresentationID$.mp4".into(),
                "https://cdn/a/s-$Time$.m4s".into()
            ))
        );
    }

    #[test]
    fn parses_ts_with_and_without_keys() {
        let t = parse_target(&format!("/pw/ts/{KEY}/https://cdn/a.mpd"), "", "pw").unwrap();
        assert_eq!((t.kind, t.key.as_deref()), (Kind::Ts, Some(KEY)));
        let t = parse_target("/pw/ts/https://cdn/a.mpd", "", "pw").unwrap();
        assert_eq!((t.kind, t.key), (Kind::Ts, None));
        let t = parse_target("/pw/m3u-ts/https://tv/iptv.m3u", "", "pw").unwrap();
        assert_eq!(t.kind, Kind::M3uTs);
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(matches!(
            parse_target("/nope/mpd/https://a/b", "", "pw"),
            Err(AppError::Auth(_))
        ));
        for path in [
            "/pw/https://a/b",
            "/pw/mpd/what/https://a/b",
            "/pw/seg/https://a/b",
        ] {
            assert!(
                matches!(parse_target(path, "", "pw"), Err(AppError::BadRequest(_))),
                "{path}"
            );
        }
    }

    #[test]
    fn redacts_password_and_keys() {
        assert_eq!(
            redacted_path(&format!("/pw/seg/{KEY}/t=aGVsbG8/https://cdn/a/seg.m4s")),
            "/…/seg/…/t=…/https://cdn/a/seg.m4s"
        );
        assert_eq!(
            redacted_path("/pw/m3u/https://tv/iptv.m3u"),
            "/…/m3u/https://tv/iptv.m3u"
        );
        let m = encode_upstream("https://cdn/a.mpd?f=1");
        assert_eq!(
            redacted_path(&format!("/pw/hls/{KEY}/{m}/v/index.m3u8")),
            "/…/hls/…/m=https://cdn/a.mpd?f=1 /v/index.m3u8"
        );
    }

    #[test]
    fn parses_encoded_upstream() {
        let m = encode_upstream("https://cdn/a.mpd?f=%28x%3D1%29");
        let t = parse_target(&format!("/pw/ts/{KEY}/{m}"), "", "pw").unwrap();
        assert_eq!(t.kind, Kind::Ts);
        assert_eq!(t.url, "https://cdn/a.mpd?f=%28x%3D1%29");
        assert!(matches!(
            parse_target(&format!("/pw/ts/{m}/extra"), "", "pw"),
            Err(AppError::BadRequest(_))
        ));
        let t = parse_target(&format!("/pw/hls/{m}/a0/index.m3u8"), "", "pw").unwrap();
        assert_eq!(t.hls_path.as_deref(), Some("a0/index.m3u8"));
    }
}
