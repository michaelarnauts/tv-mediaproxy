//! M3U playlist rewrite.
//!
//! Every ClearKey-protected MPD entry is routed through this proxy as
//! `{prefix}/mpd/{kid}:{key}/{url}`, with the keys from
//! `#KODIPROP:inputstream.adaptive.license_key`. Those entries lose their
//! `inputstream.adaptive.*` KODIPROPs: the proxied stream is no longer
//! encrypted. Everything else is passed through unchanged.

use std::collections::HashMap;

use crate::handler::is_key_list;

const ADAPTIVE_PROP: &str = "#KODIPROP:inputstream.adaptive.";

/// `prefix` is the proxy's public base URL including the password.
pub fn rewrite_m3u(content: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(content.len() + 4096);
    // Lines between two stream URLs are held back until the URL is seen,
    // since the KODIPROPs that decide the rewrite precede it.
    let mut pending: Vec<&str> = Vec::new();

    for line in content.lines() {
        let url = line.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            pending.push(line);
            continue;
        }

        let props = adaptive_props(&pending);
        let clear_key = props.get("license_type").map(String::as_str) == Some("org.w3.clearkey");
        let key = props.get("license_key").filter(|k| is_key_list(k));
        let is_mpd = props.get("manifest_type").map(String::as_str) == Some("mpd")
            || url::Url::parse(url).is_ok_and(|u| u.path().ends_with(".mpd"));

        let proxied = match key {
            Some(key) if clear_key && is_mpd => Some(format!("{prefix}/mpd/{key}/{url}")),
            _ => None,
        };
        for l in pending.drain(..) {
            if proxied.is_none() || !l.trim().starts_with(ADAPTIVE_PROP) {
                out.push_str(l);
                out.push('\n');
            }
        }
        out.push_str(proxied.as_deref().unwrap_or(line));
        out.push('\n');
    }
    for l in pending {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// `inputstream.adaptive.<name>=<value>` properties among `lines`.
fn adaptive_props(lines: &[&str]) -> HashMap<String, String> {
    lines
        .iter()
        .filter_map(|l| l.trim().strip_prefix(ADAPTIVE_PROP))
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "fa99f6cb1eb40f944b8b82507cc9ed6f:8e3265a443b9e30f47846df6ea3af11a";

    #[test]
    fn clearkey_mpd_is_proxied_and_loses_its_kodiprops() {
        let m3u = format!(
            "#EXTM3U\n\n#EXTINF:-1 tvg-id=\"a\",A\n#KODIPROP:inputstream.adaptive.manifest_type=mpd\n\
             #KODIPROP:inputstream.adaptive.license_type=org.w3.clearkey\n\
             #KODIPROP:inputstream.adaptive.license_key={KEY}\nhttps://cdn/a/.mpd?x=1\n\n\
             #EXTINF:-1,B\nhttps://cdn/b.m3u8\n"
        );
        let out = rewrite_m3u(&m3u, "http://p/pw");
        assert!(!out.contains("KODIPROP"), "{out}");
        assert!(
            out.contains(&format!(
                "#EXTINF:-1 tvg-id=\"a\",A\nhttp://p/pw/mpd/{KEY}/https://cdn/a/.mpd?x=1\n"
            )),
            "{out}"
        );
        assert!(out.contains("#EXTINF:-1,B\nhttps://cdn/b.m3u8\n"), "{out}");
    }

    #[test]
    fn unprotected_mpd_is_untouched() {
        let m3u = "#EXTM3U\n#EXTINF:-1,C\nhttps://cdn/c.mpd\n";
        assert_eq!(rewrite_m3u(m3u, "http://p/pw"), m3u);
    }
}
