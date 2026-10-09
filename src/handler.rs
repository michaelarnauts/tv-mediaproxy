//! The proxy route: `GET /{password}/{kind}/[{kid}:{key}[,…]/][t={templates}/]{upstream URL}`.
//!
//! The upstream URL is pasted as-is after the prefix (`https://…`, query
//! included). `kind` says what it is:
//!
//! - `m3u`: a playlist; ClearKey MPD entries are routed through this proxy,
//! - `mpd`: a manifest, rewritten for playback through this proxy
//!   (see `mpd::rewrite`),
//! - `seg`: an init or media segment of protected content. Both resolve from
//!   the same `BaseURL`, so the URL tells them apart: one that matches the
//!   media template is a media segment and gets decrypted with its init
//!   segment; anything else is the init segment and loses its protection.
//!
//! `t=` carries the base64url absolute initialization and media templates; the
//! MPD rewrite puts it in the `BaseURL` of protected content, so the segment
//! URLs a player builds lead back to their init segment (`mpd::template`).

use std::sync::Arc;

use actix_web::{web, HttpRequest, HttpResponse};
use base64::Engine;
use reqwest::header::HeaderMap;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    m3u::rewrite_m3u,
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
    M3u,
    Mpd,
    Seg,
}

#[derive(Debug, PartialEq)]
struct Target {
    kind: Kind,
    /// `kid:key[,kid:key]`, hex.
    key: Option<String>,
    /// Absolute (initialization, media) templates of protected content.
    templates: Option<(String, String)>,
    url: String,
}

pub async fn proxy_handler(
    req: HttpRequest,
    stream_manager: web::Data<StreamManager>,
    config: web::Data<Arc<Config>>,
    metrics: web::Data<Arc<AppMetrics>>,
    init_cache: web::Data<InitCache>,
) -> AppResult<HttpResponse> {
    metrics.inc_request();
    let password = &config.auth.api_password;
    let target = parse_target(req.uri().path(), req.query_string(), password)?;
    let prefix = format!(
        "{}/{password}",
        public_proxy_base_url(&req, &config.server.path)
    );

    let (body, content_type) = match target.kind {
        Kind::M3u => {
            let playlist = stream_manager
                .fetch_bytes(target.url.clone(), HeaderMap::new())
                .await?;
            let rewritten = rewrite_m3u(&String::from_utf8_lossy(&playlist), &prefix);
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
    let usage = || AppError::BadRequest("Expected /{password}/{m3u|mpd|seg}/…/https://…".into());
    let path = path.strip_prefix('/').unwrap_or(path);
    let (given, rest) = path.split_once('/').unwrap_or((path, ""));
    if given != password {
        return Err(AppError::Auth("Invalid password".into()));
    }
    let (kind, mut rest) = rest.split_once('/').ok_or_else(usage)?;
    let kind = match kind {
        "m3u" => Kind::M3u,
        "mpd" => Kind::Mpd,
        "seg" => Kind::Seg,
        _ => return Err(usage()),
    };

    let mut target = Target {
        kind,
        key: None,
        templates: None,
        url: String::new(),
    };
    while !is_upstream_url(rest) {
        let (segment, tail) = rest.split_once('/').ok_or_else(usage)?;
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
    }
}
