//! `GET /{kind}/{upstream URL}`, `/{mpd|ts}/{keys}/{upstream URL}` or
//! `/seg/{keys}/t={templates}/{upstream URL}`, where `{keys}` is
//! `{kid}:{key}[,…]` or `-` for none.
//!
//! The upstream URL is pasted as-is (`https://…`, query included) or encoded
//! as `m={base64url}`. `kind` says what it is:
//!
//! - `m3u`: a playlist; ClearKey MPD entries are routed through `mpd`,
//! - `m3u-ts`: the same, routing ClearKey and unprotected MPD entries
//!   through `ts`,
//! - `mpd`: a manifest, rewritten for playback through this proxy
//!   (see `mpd::rewrite`),
//! - `seg`: an init or media segment of protected content. Both resolve from
//!   the same `BaseURL`, so the URL tells them apart: one that matches the
//!   media template is a media segment and gets decrypted with its init
//!   segment; anything else is the init segment and loses its protection,
//! - `ts`: a manifest played into one MPEG-TS stream by GPAC (see `ts`),
//! - `hdhr/m3u`: an HDHomeRun tuner whose channels are the playlist's `ts`
//!   entries (see `hdhr`); the client appends `/discover.json` and the like.
//!   `hdhr/guide/{playlist URL}` is its guide.
//!
//! `t=` carries the base64url absolute initialization and media templates; the
//! MPD rewrite puts it in the `BaseURL` of protected content, so the segment
//! URLs a player builds lead back to their init segment (`mpd::template`).

use actix_web::http::Uri;
use actix_web::{web, HttpRequest, HttpResponse};
use base64::Engine;
use bytes::Bytes;

use crate::drm::cenc;
use crate::error::{AppError, AppResult};
use crate::hdhr;
use crate::m3u::{rewrite_m3u, Output};
use crate::mpd::rewrite::{rewrite_mpd, RewriteParams};
use crate::mpd::template::init_for_segment;

/// Raw upstream init segments, keyed by URL.
#[derive(Clone)]
pub struct InitCache(pub moka::future::Cache<String, Bytes>);

#[derive(Debug, PartialEq)]
enum Kind {
    Hdhr,
    HdhrGuide,
    M3u,
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
    url: String,
}

pub async fn handle(
    req: HttpRequest,
    client: web::Data<reqwest::Client>,
    init_cache: web::Data<InitCache>,
) -> AppResult<HttpResponse> {
    let target = parse_target(req.uri())?;
    let prefix = {
        let info = req.connection_info();
        format!("{}://{}", info.scheme(), info.host())
    };

    let (body, content_type): (Bytes, _) = match target.kind {
        Kind::Ts => {
            let stream = crate::ts::play(&client, &target.url, target.key.as_deref()).await?;
            return Ok(HttpResponse::Ok()
                .content_type("video/mp2t")
                .insert_header(("cache-control", "no-cache"))
                .streaming(stream));
        }
        Kind::Hdhr => {
            let (playlist, file) = hdhr::split(&target.url);
            let base = format!("{prefix}/hdhr/m3u/{playlist}");
            let id = hdhr::device_id(playlist);
            match file {
                hdhr::File::Discover => (hdhr::discover(&base, &id).into(), "application/json"),
                hdhr::File::LineupStatus => (hdhr::lineup_status().into(), "application/json"),
                hdhr::File::DeviceXml => (hdhr::device_xml(&base, &id).into(), "application/xml"),
                hdhr::File::Lineup => {
                    let (_, channels) = lineup(&client, playlist, &prefix).await?;
                    (hdhr::lineup(&channels).into(), "application/json")
                }
            }
        }
        Kind::HdhrGuide => {
            let (m3u, channels) = lineup(&client, &target.url, &prefix).await?;
            let url = hdhr::guide_url(&m3u).ok_or_else(|| {
                AppError::NotFound("The playlist names no guide (x-tvg-url)".into())
            })?;
            let xml = gunzip(fetch(&client, &url).await?.body)
                .map_err(|e| AppError::Upstream(format!("Guide {url}: {e}")))?;
            let guide = hdhr::guide(&xml, &channels)
                .map_err(|e| AppError::Upstream(format!("Guide {url}: {e}")))?;
            (guide.into(), "application/xml")
        }
        Kind::M3u | Kind::M3uTs => {
            let output = if target.kind == Kind::M3u {
                Output::Mpd
            } else {
                Output::Ts
            };
            let playlist = fetch(&client, &target.url).await?.body;
            let rewritten = rewrite_m3u(&String::from_utf8_lossy(&playlist), &prefix, output);
            (rewritten.into(), "audio/x-mpegurl; charset=utf-8")
        }
        Kind::Mpd => {
            let fetched = fetch(&client, &target.url).await?;
            let params = RewriteParams {
                manifest_url: &fetched.url,
                prefix: &prefix,
                key: target.key.as_deref(),
            };
            let rewritten = rewrite_mpd(&fetched.body, &params)
                .map_err(|e| AppError::Upstream(format!("MPD from {}: {e}", fetched.url)))?;
            (rewritten.into(), "application/dash+xml")
        }
        Kind::Seg => {
            let keys = target.key.as_deref().unwrap_or_default();
            let (init_template, media_template) = target.templates.as_ref().ok_or_else(|| {
                AppError::BadRequest("seg needs t= (the segment templates)".into())
            })?;
            let body = match init_for_segment(init_template, media_template, &target.url) {
                Some(init_url) => {
                    let (init, segment) = futures::try_join!(
                        fetch_init(&client, &init_cache, init_url),
                        fetch(&client, &target.url),
                    )?;
                    cenc::decrypt_segment(&init, &segment.body, keys)
                }
                None => {
                    let init = fetch_init(&client, &init_cache, target.url.clone()).await?;
                    cenc::decrypt_init(&init, keys)
                }
            }
            .map_err(AppError::BadRequest)?;
            (body.into(), "video/mp4")
        }
    };

    Ok(HttpResponse::Ok()
        .content_type(content_type)
        .insert_header(("cache-control", "no-cache"))
        .body(body))
}

pub struct Fetched {
    pub body: Bytes,
    /// URL after redirects.
    pub url: String,
}

pub async fn fetch(client: &reqwest::Client, url: &str) -> AppResult<Fetched> {
    let failed = |e: reqwest::Error| AppError::Upstream(format!("{url}: {e}"));
    let response = client.get(url).send().await.map_err(failed)?;
    let status = response.status();
    // A segment that is not available (yet) stays a 404 for the player.
    if matches!(status.as_u16(), 404 | 410) {
        return Err(AppError::NotFound(format!("Upstream {status} for {url}")));
    }
    if !status.is_success() {
        return Err(AppError::Upstream(format!("Upstream {status} for {url}")));
    }
    let final_url = response.url().to_string();
    Ok(Fetched {
        body: response.bytes().await.map_err(failed)?,
        url: final_url,
    })
}

/// The playlist at `url` and its channels as an HDHomeRun lineup.
async fn lineup(
    client: &reqwest::Client,
    url: &str,
    prefix: &str,
) -> AppResult<(String, Vec<hdhr::Channel>)> {
    let m3u = String::from_utf8_lossy(&fetch(client, url).await?.body).into_owned();
    let routed = rewrite_m3u(&m3u, prefix, Output::Ts);
    let channels = hdhr::channels(&routed, &format!("{prefix}/ts/"));
    Ok((m3u, channels))
}

/// An init segment, from the cache when its representation was seen before.
async fn fetch_init(client: &reqwest::Client, cache: &InitCache, url: String) -> AppResult<Bytes> {
    cache
        .0
        .try_get_with(url.clone(), async { Ok(fetch(client, &url).await?.body) })
        .await
        .map_err(|e: std::sync::Arc<AppError>| match e.as_ref() {
            AppError::NotFound(m) => AppError::NotFound(m.clone()),
            other => AppError::Upstream(other.to_string()),
        })
}

/// `data` unpacked if it is a gzip file (`.xml.gz` guides).
fn gunzip(data: Bytes) -> std::io::Result<Bytes> {
    if !data.starts_with(&[0x1f, 0x8b]) {
        return Ok(data);
    }
    let mut out = Vec::with_capacity(data.len() * 8);
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&data[..]), &mut out)?;
    Ok(out.into())
}

/// Split the request into kind, keys, templates and upstream URL.
fn parse_target(uri: &Uri) -> AppResult<Target> {
    let bad = |what: &str| {
        AppError::BadRequest(format!(
            "Bad {what}: expected /{{m3u|m3u-ts|hdhr/m3u|hdhr/guide}}/<url>, \
             /{{mpd|ts}}/<keys|->/<url> or /seg/<keys>/t=<templates>/<url>"
        ))
    };
    let mut rest = uri.path().trim_start_matches('/');
    let kind = match next(&mut rest) {
        "hdhr" => match next(&mut rest) {
            "m3u" => Kind::Hdhr,
            "guide" => Kind::HdhrGuide,
            _ => return Err(bad("kind")),
        },
        "m3u" => Kind::M3u,
        "m3u-ts" => Kind::M3uTs,
        "mpd" => Kind::Mpd,
        "seg" => Kind::Seg,
        "ts" => Kind::Ts,
        _ => return Err(bad("kind")),
    };
    let key = match next_if(&mut rest, matches!(kind, Kind::Mpd | Kind::Seg | Kind::Ts)) {
        None | Some("-") => None,
        Some(keys) => Some(
            urlencoding::decode(keys)
                .ok()
                .filter(|k| is_key_list(k))
                .ok_or_else(|| bad("keys"))?
                .into_owned(),
        ),
    };
    let templates = match next_if(&mut rest, kind == Kind::Seg) {
        None => None,
        Some(t) => Some(
            t.strip_prefix("t=")
                .and_then(decode_base64)
                .and_then(|s| {
                    s.split_once('\n')
                        .map(|(i, m)| (i.to_string(), m.to_string()))
                })
                .ok_or_else(|| bad("t="))?,
        ),
    };

    let url = if let Some(encoded) = rest.strip_prefix("m=") {
        decode_base64(encoded)
            .filter(|u| is_upstream_url(u))
            .ok_or_else(|| bad("m="))?
    } else if is_upstream_url(rest) {
        // Accept `https:/host` too: some clients and proxies collapse `//`.
        let (scheme, after) = rest.split_once(':').unwrap_or_default();
        let mut url = format!("{scheme}://{}", after.trim_start_matches('/'));
        if let Some(query) = uri.query() {
            url.push('?');
            url.push_str(query);
        }
        url
    } else {
        return Err(bad("upstream URL"));
    };
    Ok(Target {
        kind,
        key,
        templates,
        url,
    })
}

/// The next path segment, taken off `rest`.
fn next<'a>(rest: &mut &'a str) -> &'a str {
    let (segment, tail) = rest.split_once('/').unwrap_or((rest, ""));
    *rest = tail;
    segment
}

fn next_if<'a>(rest: &mut &'a str, wanted: bool) -> Option<&'a str> {
    wanted.then(|| next(rest))
}

/// What a request asks for, for the access log: kind and upstream URL, no keys.
pub fn log_target(uri: &Uri) -> String {
    match parse_target(uri) {
        Ok(t) => format!("{:?} {}", t.kind, t.url),
        Err(_) => uri.path().to_string(),
    }
}

fn decode_base64(encoded: &str) -> Option<String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// `m=` followed by `url`, base64url: an upstream URL in a path, so that
/// clients that normalise query strings (Plex) cannot change what reaches it.
pub fn encode_upstream(url: &str) -> String {
    format!(
        "m={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url)
    )
}

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

    fn parse(uri: &str) -> AppResult<Target> {
        parse_target(&uri.parse().unwrap())
    }

    #[test]
    fn parses_playlist_url() {
        let t = parse("/m3u/https://tv.example/iptv.m3u").unwrap();
        assert_eq!(t.kind, Kind::M3u);
        assert_eq!(t.url, "https://tv.example/iptv.m3u");
        assert_eq!(t.key, None);
    }

    #[test]
    fn parses_segment_with_key_templates_query_and_collapsed_slashes() {
        let templates = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://cdn/a/i-$RepresentationID$.mp4\nhttps://cdn/a/s-$Time$.m4s");
        let t = parse(&format!(
            "/seg/{KEY}/t={templates}/https:/cdn/a/s-1.m4s?m=1"
        ))
        .unwrap();
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
    fn parses_encoded_upstream() {
        let m = encode_upstream("https://cdn/a.mpd?f=%28x%3D1%29");
        let t = parse(&format!("/ts/{KEY}/{m}")).unwrap();
        assert_eq!((t.kind, t.key.as_deref()), (Kind::Ts, Some(KEY)));
        let t = parse(&format!("/ts/-/{m}")).unwrap();
        assert_eq!((t.kind, t.key), (Kind::Ts, None));
        assert_eq!(t.url, "https://cdn/a.mpd?f=%28x%3D1%29");
        assert_eq!(
            log_target(&format!("/ts/{KEY}/{m}").parse().unwrap()),
            "Ts https://cdn/a.mpd?f=%28x%3D1%29"
        );
    }
}
