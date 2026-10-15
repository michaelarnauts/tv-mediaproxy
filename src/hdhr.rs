//! HDHomeRun emulation, for Plex Live TV, which only takes HDHomeRun tuners.
//!
//! The tuner's base URL is `{prefix}/hdhr/{playlist URL}`; a client appends
//! `/discover.json`, `/lineup.json`, `/lineup_status.json` or `/device.xml`.
//! The lineup lists the playlist's entries that go through `ts`, so every
//! channel is one continuous MPEG-TS stream, as from a real tuner.
//!
//! `/guide.xml` is the playlist's XMLTV guide made to match the lineup:
//! HDHomeRun lineups carry no guide ids, so clients match a guide by channel
//! number. Each guide channel the lineup has (by `tvg-id`) gets the
//! playlist's name, number and logo; the rest is left out.

use std::collections::HashMap;
use std::sync::LazyLock;

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use regex::Regex;
use serde_json::json;

#[derive(Debug, PartialEq)]
pub enum File {
    Discover,
    Lineup,
    LineupStatus,
    DeviceXml,
    Guide,
}

/// Split a tuner request into the playlist URL and the file asked for. The
/// base URL itself answers as `discover.json`.
pub fn split(url: &str) -> (&str, File) {
    for (name, file) in [
        ("/discover.json", File::Discover),
        ("/lineup.json", File::Lineup),
        ("/lineup_status.json", File::LineupStatus),
        ("/device.xml", File::DeviceXml),
        ("/guide.xml", File::Guide),
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
    /// `tvg-id`: the channel's id in the guide.
    id: Option<String>,
    logo: Option<String>,
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
                id: attr("tvg-id").filter(|i| !i.is_empty()),
                logo: attr("tvg-logo").filter(|l| !l.is_empty()),
            });
        }
    }
    out
}

/// The guide URL a playlist announces in its header (`x-tvg-url`).
pub fn guide_url(m3u: &str) -> Option<String> {
    let header = m3u.lines().next()?;
    ["x-tvg-url", "url-tvg"].iter().find_map(|key| {
        let start = header.find(&format!("{key}=\""))? + key.len() + 2;
        let end = header[start..].find('"')? + start;
        Some(header[start..end].to_string()).filter(|u| !u.is_empty())
    })
}

/// `xml`, an XMLTV guide, reduced to the lineup's channels, whose names,
/// numbers and logos come from the playlist.
pub fn guide(xml: &[u8], channels: &[Channel]) -> Result<Vec<u8>, String> {
    let mut by_id: HashMap<&str, Vec<&Channel>> = HashMap::new();
    for channel in channels {
        if let Some(id) = &channel.id {
            by_id.entry(id).or_default().push(channel);
        }
    }
    let mut reader = Reader::from_reader(xml);
    let mut writer = Writer::new(Vec::with_capacity(xml.len()));
    let write = |writer: &mut Writer<Vec<u8>>, event: Event| {
        writer.write_event(event).map_err(|e| e.to_string())
    };
    // Depth inside an element being left out.
    let mut skip = 0usize;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("XMLTV at {}: {e}", reader.buffer_position()))?;
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                Event::Eof => break,
                _ => {}
            }
            continue;
        }
        let (start, open) = match &event {
            Event::Start(start) => (start, true),
            Event::Empty(start) => (start, false),
            Event::Eof => break,
            _ => {
                write(&mut writer, event)?;
                continue;
            }
        };
        match start.local_name().as_ref() {
            b"channel" => {
                let id = attr(start, b"id").unwrap_or_default();
                if let Some(ours) = by_id.get(id.as_str()) {
                    write_channel(&mut writer, &id, ours)?;
                }
                skip = usize::from(open);
            }
            b"programme"
                if !by_id.contains_key(attr(start, b"channel").unwrap_or_default().as_str()) =>
            {
                skip = usize::from(open);
            }
            _ => write(&mut writer, event)?,
        }
    }
    Ok(writer.into_inner())
}

fn attr(start: &BytesStart, name: &[u8]) -> Option<String> {
    start
        .try_get_attribute(name)
        .ok()
        .flatten()
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn write_channel(writer: &mut Writer<Vec<u8>>, id: &str, ours: &[&Channel]) -> Result<(), String> {
    let mut events = vec![Event::Start(
        BytesStart::new("channel").with_attributes([("id", id)]),
    )];
    let mut names: Vec<&str> = Vec::new();
    for channel in ours {
        for name in [channel.name.as_str(), channel.number.as_str()] {
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    for name in names {
        events.push(Event::Start(BytesStart::new("display-name")));
        events.push(Event::Text(BytesText::new(name)));
        events.push(Event::End(BytesEnd::new("display-name")));
    }
    if let Some(logo) = ours.iter().find_map(|c| c.logo.as_deref()) {
        events.push(Event::Empty(
            BytesStart::new("icon").with_attributes([("src", logo)]),
        ));
    }
    events.push(Event::End(BytesEnd::new("channel")));
    for event in events {
        writer.write_event(event).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn discover(base: &str, device_id: &str) -> String {
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
        "TunerCount": crate::env("HDHR_TUNERS", 6),
    })
    .to_string()
}

/// Entries as a real HDHomeRun writes them, codecs included. The codecs are
/// assumed: H.264 and AAC, what the sources carry and `ts` passes through.
pub fn lineup(channels: &[Channel]) -> String {
    let entries: Vec<_> = channels
        .iter()
        .map(|c| {
            json!({
                "GuideNumber": c.number,
                "GuideName": c.name,
                "VideoCodec": "H264",
                "AudioCodec": "AAC",
                "HD": 1,
                "URL": c.url,
            })
        })
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
            http://p/ts/https://cdn/a.mpd\n\
            #EXTINF:-1,Channel B\nhttp://p/ts/https://cdn/b.mpd\n\
            #EXTINF:-1,Radio C\nhttps://cdn/c.mp3\n";
        let channels = channels(m3u, "http://p/ts/");
        assert_eq!(
            lineup(&channels),
            r#"[{"AudioCodec":"AAC","GuideName":"Channel A","GuideNumber":"7","HD":1,"URL":"http://p/ts/https://cdn/a.mpd","VideoCodec":"H264"},{"AudioCodec":"AAC","GuideName":"Channel B","GuideNumber":"2","HD":1,"URL":"http://p/ts/https://cdn/b.mpd","VideoCodec":"H264"}]"#
        );
    }

    #[test]
    fn guide_takes_names_numbers_and_logos_from_the_playlist() {
        let m3u = "#EXTM3U x-tvg-url=\"https://tv/guide.xml.gz\"\n\
            #EXTINF:-1 tvg-id=\"a.be\" tvg-chno=\"7\" tvg-logo=\"https://tv/a.png\",Channel A\n\
            http://p/ts/m=YQ\n";
        assert_eq!(guide_url(m3u).as_deref(), Some("https://tv/guide.xml.gz"));
        let xml = r#"<?xml version="1.0"?>
<tv><channel id="a.be"><display-name>A old</display-name><icon src="old.png"/></channel>
<channel id="b.be"><display-name>B</display-name></channel>
<programme start="1" stop="2" channel="a.be"><title>Show &amp; tell</title></programme>
<programme start="1" stop="2" channel="b.be"><title>Gone</title></programme></tv>"#;
        let out = String::from_utf8(guide(xml.as_bytes(), &channels(m3u, "http://p/ts/")).unwrap())
            .unwrap();
        assert!(out.contains(r#"<channel id="a.be"><display-name>Channel A</display-name><display-name>7</display-name><icon src="https://tv/a.png"/></channel>"#), "{out}");
        assert!(out.contains("<title>Show &amp; tell</title>"), "{out}");
        assert!(
            !out.contains("b.be") && !out.contains("Gone") && !out.contains("A old"),
            "{out}"
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
