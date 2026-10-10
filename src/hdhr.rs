//! HDHomeRun emulation, for Plex Live TV, which only takes HDHomeRun tuners.
//!
//! The tuner's base URL is `{prefix}/hdhr/{playlist URL}`; a client appends
//! `/discover.json`, `/lineup.json`, `/lineup_status.json` or `/device.xml`.
//! The lineup lists the playlist's entries that go through `ts`, so every
//! channel is one continuous MPEG-TS stream, as from a real tuner.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::json;

#[derive(Debug, PartialEq)]
pub enum File {
    Discover,
    Lineup,
    LineupStatus,
    DeviceXml,
}

/// Split a tuner request into the playlist URL and the file asked for. The
/// base URL itself answers as `discover.json`.
pub fn split(url: &str) -> (&str, File) {
    for (name, file) in [
        ("/discover.json", File::Discover),
        ("/lineup.json", File::Lineup),
        ("/lineup_status.json", File::LineupStatus),
        ("/device.xml", File::DeviceXml),
    ] {
        if let Some(playlist) = url.strip_suffix(name) {
            return (playlist, file);
        }
    }
    (url, File::Discover)
}

pub struct Channel {
    number: String,
    name: String,
    url: String,
}

/// The channels of a playlist rewritten for `ts` (see `m3u`): the entries
/// whose URL starts with `ts_prefix`, numbered by `tvg-chno` or position.
pub fn channels(m3u: &str, ts_prefix: &str) -> Vec<Channel> {
    static ATTR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"([A-Za-z-]+)="([^"]*)""#).expect("valid regex"));
    let mut out = Vec::new();
    let mut extinf: Option<&str> = None;
    for line in m3u.lines().map(str::trim) {
        if line.starts_with("#EXTINF") {
            extinf = Some(line);
        } else if !line.starts_with('#') && !line.is_empty() {
            let Some(info) = extinf.take() else { continue };
            if !line.starts_with(ts_prefix) {
                continue;
            }
            // Attributes sit between `#EXTINF:-1` and the comma before the name.
            let (attrs, name) = info.rsplit_once(',').unwrap_or((info, ""));
            let attr = |key: &str| {
                ATTR.captures_iter(attrs)
                    .find(|c| &c[1] == key)
                    .map(|c| c[2].to_string())
            };
            out.push(Channel {
                number: attr("tvg-chno").unwrap_or_else(|| (out.len() + 1).to_string()),
                name: name.trim().to_string(),
                url: line.to_string(),
            });
        }
    }
    out
}

pub fn discover(base: &str, device_id: &str, tuners: usize) -> String {
    json!({
        "FriendlyName": "tv-mediaproxy",
        "Manufacturer": "Silicondust",
        "ModelNumber": "HDTC-2US",
        "FirmwareName": "hdhomeruntc_atsc",
        "FirmwareVersion": "20200101",
        "DeviceID": device_id,
        "DeviceAuth": "tv-mediaproxy",
        "BaseURL": base,
        "LineupURL": format!("{base}/lineup.json"),
        "TunerCount": tuners,
    })
    .to_string()
}

pub fn lineup(channels: &[Channel]) -> String {
    let entries: Vec<_> = channels
        .iter()
        .map(|c| json!({ "GuideNumber": c.number, "GuideName": c.name, "URL": c.url }))
        .collect();
    serde_json::Value::Array(entries).to_string()
}

pub fn lineup_status() -> String {
    json!({
        "ScanInProgress": 0,
        "ScanPossible": 1,
        "Source": "Cable",
        "SourceList": ["Cable"],
    })
    .to_string()
}

pub fn device_xml(base: &str, device_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <URLBase>{base}</URLBase>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>tv-mediaproxy</friendlyName>
    <manufacturer>Silicondust</manufacturer>
    <modelName>HDTC-2US</modelName>
    <modelNumber>HDTC-2US</modelNumber>
    <serialNumber>{device_id}</serialNumber>
    <UDN>uuid:{device_id}</UDN>
  </device>
</root>
"#
    )
}

/// A stable 8-digit hex id for a playlist URL (FNV-1a), so a tuner keeps its
/// identity across restarts and upgrades.
pub fn device_id(playlist: &str) -> String {
    let hash = playlist.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    format!("{hash:08X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_file_from_the_playlist() {
        assert_eq!(
            split("https://tv/api/playlist/lineup.json"),
            ("https://tv/api/playlist", File::Lineup)
        );
        assert_eq!(
            split("https://tv/api/playlist"),
            ("https://tv/api/playlist", File::Discover)
        );
    }

    #[test]
    fn lineup_takes_ts_entries_only() {
        let m3u = "#EXTM3U\n\
            #EXTINF:-1 tvg-id=\"a.be\" tvg-chno=\"7\" group-title=\"TV\",Channel A\n\
            http://p/pw/ts/https://cdn/a.mpd\n\
            #EXTINF:-1,Channel B\nhttp://p/pw/ts/https://cdn/b.mpd\n\
            #EXTINF:-1,Radio C\nhttps://cdn/c.mp3\n";
        let channels = channels(m3u, "http://p/pw/ts/");
        assert_eq!(
            lineup(&channels),
            r#"[{"GuideName":"Channel A","GuideNumber":"7","URL":"http://p/pw/ts/https://cdn/a.mpd"},{"GuideName":"Channel B","GuideNumber":"2","URL":"http://p/pw/ts/https://cdn/b.mpd"}]"#
        );
    }

    #[test]
    fn device_id_is_stable() {
        assert_eq!(
            device_id("https://tv/api/playlist"),
            device_id("https://tv/api/playlist")
        );
        assert_eq!(device_id("").len(), 8);
    }
}
