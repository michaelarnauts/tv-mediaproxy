//! HLS out of a DASH manifest, so HLS players get the origin's whole window.
//!
//! The master playlist offers the best video representation as the variant
//! and every audio adaptation set as an audio rendition. A media playlist lists
//! one rendition's segments across all periods: `EXT-X-DISCONTINUITY` between
//! periods, `EXT-X-MAP` for each period's init segment.
//!
//! Media playlists and segments live under one directory per rendition,
//! `…/m=<manifest>/<rendition>/`, so segment URIs are short relative names:
//! `<period>/init.mp4` and `<period>/<number or time>.m4s`. A segment request
//! is resolved against the manifest (see `resolve`), fetched and, when
//! protected, decrypted. Periods are named by their start in milliseconds.

use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use dash_mpd_core::{
    is_audio_adaptation, is_video_adaptation, AdaptationSet, BaseURL, Period, Representation,
    SegmentTemplate, MPD,
};
use url::Url;

use crate::mpd::template::fill_segment;

pub struct HlsParams<'a> {
    /// The manifest as requested: it names the rendition directories.
    pub source_url: &'a str,
    /// This proxy's public base URL including the password.
    pub prefix: &'a str,
    /// `kid:key[,kid:key]`.
    pub key: Option<&'a str>,
}

/// An init or media segment at the origin.
pub struct Resource {
    pub url: String,
    /// The init segment that goes with a media segment; `None` for an init.
    pub init_url: Option<String>,
    pub protected: bool,
}

/// The master playlist.
pub fn master_playlist(mpd: &MPD, params: &HlsParams) -> Result<String, String> {
    let period = mpd.periods.last().ok_or("no Period")?;
    let audio = audio_sets(period);
    let video = best_video(period);

    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let mut audio_bandwidth = 0;
    let mut audio_codecs = None;
    for (i, set) in audio.iter().enumerate() {
        let Some(rep) = best(set) else { continue };
        audio_bandwidth = audio_bandwidth.max(rep.bandwidth.unwrap_or(0));
        audio_codecs = audio_codecs.or_else(|| codecs(set, rep));
        let lang = set.lang.clone().or_else(|| rep.lang.clone());
        let name = format!("{} {}", lang.as_deref().unwrap_or("audio"), i + 1);
        let _ = write!(
            out,
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{name}\""
        );
        if let Some(lang) = &lang {
            let _ = write!(out, ",LANGUAGE=\"{lang}\"");
        }
        let default = if i == 0 { "YES" } else { "NO" };
        let uri = playlist_url(params, &format!("a{i}"));
        let _ = writeln!(out, ",DEFAULT={default},AUTOSELECT=YES,URI=\"{uri}\"");
    }

    // Only the best video representation: ffmpeg opens every variant (and
    // fetches its segments) to probe it, whatever its BANDWIDTH.
    if let Some((set, rep)) = video {
        let bandwidth = rep.bandwidth.unwrap_or(0) + audio_bandwidth;
        let _ = write!(out, "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}");
        if let (Some(w), Some(h)) = (rep.width.or(set.width), rep.height.or(set.height)) {
            let _ = write!(out, ",RESOLUTION={w}x{h}");
        }
        if let Some(rate) = rep
            .frameRate
            .as_deref()
            .or(set.frameRate.as_deref())
            .and_then(frame_rate)
        {
            let _ = write!(out, ",FRAME-RATE={rate:.3}");
        }
        let all_codecs: Vec<String> = codecs(set, rep).into_iter().chain(audio_codecs).collect();
        if !all_codecs.is_empty() {
            let _ = write!(out, ",CODECS=\"{}\"", all_codecs.join(","));
        }
        if !audio.is_empty() {
            out.push_str(",AUDIO=\"audio\"");
        }
        let _ = writeln!(out, "\n{}", playlist_url(params, "v"));
    } else {
        // Radio: the audio renditions are the variants.
        for (i, set) in audio.iter().enumerate() {
            let Some(rep) = best(set) else { continue };
            let _ = writeln!(
                out,
                "#EXT-X-STREAM-INF:BANDWIDTH={}{}\n{}",
                rep.bandwidth.unwrap_or(0),
                codecs(set, rep)
                    .map(|c| format!(",CODECS=\"{c}\""))
                    .unwrap_or_default(),
                playlist_url(params, &format!("a{i}"))
            );
        }
    }
    Ok(out)
}

/// The media playlist of `rendition` (`v`, or `a<index>`), with segment
/// URIs relative to it.
pub fn media_playlist(mpd: &MPD, rendition: &str, manifest_url: &str) -> Result<String, String> {
    let live = mpd.mpdtype.as_deref() == Some("dynamic");
    let ast = availability_start(mpd);

    let mut body = String::new();
    let mut first_seq = None;
    let mut target = 1.0_f64;
    for track in tracks(mpd, rendition, manifest_url)? {
        let template = &track.template;
        let timescale = template.timescale.unwrap_or(1).max(1);
        let pto = template.presentationTimeOffset.unwrap_or(0);
        let segments = track_segments(mpd, &track);
        if segments.is_empty() {
            continue;
        }

        // Without segment numbers, number segments on the grid of the
        // period's video segments: stable across reloads, and the same for
        // audio, whose segments are rarely all the same length.
        let by_number = template.startNumber.is_some() || track.media.contains("$Number");
        let grid = video_grid(track.period)
            .unwrap_or_else(|| nominal_duration(&segments) as f64 / timescale as f64);
        let seq = |s: &Segment| {
            if by_number {
                s.number
            } else {
                let presentation =
                    track.start + s.time.saturating_sub(pto) as f64 / timescale as f64;
                (presentation / grid).round() as u64
            }
        };
        if first_seq.is_none() {
            first_seq = Some(seq(&segments[0]));
        } else {
            body.push_str("#EXT-X-DISCONTINUITY\n");
        }
        let _ = writeln!(body, "#EXT-X-MAP:URI=\"{}/init.mp4\"", track.key);
        if let Some(ast) = ast {
            let secs = ast
                + track.start
                + (segments[0].time.saturating_sub(pto)) as f64 / timescale as f64;
            if let Some(date) = chrono::DateTime::from_timestamp_millis((secs * 1000.0) as i64) {
                let _ = writeln!(
                    body,
                    "#EXT-X-PROGRAM-DATE-TIME:{}",
                    date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                );
            }
        }
        let naming = Naming::of(&track.media);
        for s in &segments {
            let duration = s.duration as f64 / timescale as f64;
            target = target.max(duration.round());
            let _ = writeln!(
                body,
                "#EXTINF:{duration:.3},\n{}/{}.m4s",
                track.key,
                naming.name(s.number, s.time)
            );
        }
    }

    let first_seq = first_seq.ok_or_else(|| format!("no segments for rendition {rendition:?}"))?;
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{first_seq}\n",
        target as u64
    );
    if !live {
        out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    }
    out.push_str(&body);
    if !live {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    Ok(out)
}

/// When the newest available segment of a live manifest ends, in seconds
/// since the epoch: of the best video, else the first audio rendition.
pub fn live_edge(mpd: &MPD, manifest_url: &str) -> Option<f64> {
    let ast = availability_start(mpd)?;
    let rendition = if best_video(mpd.periods.last()?).is_some() {
        "v"
    } else {
        "a0"
    };
    let track = tracks(mpd, rendition, manifest_url).ok()?.pop()?;
    let last = track_segments(mpd, &track).pop()?;
    let timescale = track.template.timescale.unwrap_or(1).max(1) as f64;
    let pto = track.template.presentationTimeOffset.unwrap_or(0);
    Some(ast + track.start + (last.time + last.duration).saturating_sub(pto) as f64 / timescale)
}

/// The origin address of `path` (`<period>/init.mp4` or
/// `<period>/<name>.m4s`) in `rendition`.
pub fn resolve(
    mpd: &MPD,
    rendition: &str,
    path: &str,
    manifest_url: &str,
) -> Result<Resource, String> {
    let (period_key, file) = path.split_once('/').ok_or("bad segment path")?;
    let period_key: u64 = period_key.parse().map_err(|_| "bad period")?;
    let track = tracks(mpd, rendition, manifest_url)?
        .into_iter()
        .find(|t| t.key == period_key)
        .ok_or("period no longer in the manifest")?;
    let rep_id = track.rep.id.as_deref().unwrap_or_default();
    let bandwidth = track.rep.bandwidth.unwrap_or(0);
    let init_url = fill_segment(&track.init, rep_id, bandwidth, 0, 0);
    if file == "init.mp4" {
        return Ok(Resource {
            url: init_url,
            init_url: None,
            protected: track.protected,
        });
    }
    let name = file.strip_suffix(".m4s").ok_or("bad segment name")?;
    let (number, time) = Naming::of(&track.media).parse(name)?;
    Ok(Resource {
        url: fill_segment(&track.media, rep_id, bandwidth, number, time),
        init_url: Some(init_url),
        protected: track.protected,
    })
}

/// One rendition in one period.
struct Track<'a> {
    period: &'a Period,
    /// The period's start in milliseconds: its directory name.
    key: u64,
    /// Start and end of the period, in seconds from the availability start.
    start: f64,
    end: Option<f64>,
    rep: &'a Representation,
    template: SegmentTemplate,
    /// Absolute initialization and media templates.
    init: String,
    media: String,
    protected: bool,
}

/// `rendition` in every period that has it.
fn tracks<'a>(mpd: &'a MPD, rendition: &str, manifest_url: &str) -> Result<Vec<Track<'a>>, String> {
    let manifest = Url::parse(manifest_url).map_err(|e| format!("bad manifest URL: {e}"))?;
    let mpd_base = join(&manifest, &mpd.base_url);
    let starts = period_starts(mpd);
    let audio_ref = mpd.periods.last().map(audio_sets).unwrap_or_default();

    let mut out = Vec::new();
    for (index, period) in mpd.periods.iter().enumerate() {
        let Some((set, rep)) = find_rendition(period, rendition, &audio_ref) else {
            continue;
        };
        let template = rendition_template(period, set, rep);
        let (Some(media), Some(init)) = (&template.media, &template.initialization) else {
            tracing::warn!(
                "Representation {:?} has no SegmentTemplate; skipped",
                rep.id
            );
            continue;
        };
        let base = join(
            &join(&join(&mpd_base, &period.BaseURL), &set.BaseURL),
            &rep.BaseURL,
        );
        let absolute = |t: &str| base.join(t).map(String::from).unwrap_or_default();
        let start = starts[index];
        out.push(Track {
            period,
            key: (start * 1000.0).round() as u64,
            start,
            end: starts
                .get(index + 1)
                .copied()
                .or_else(|| period.duration.map(|d| start + d.as_secs_f64())),
            rep,
            init: absolute(init),
            media: absolute(media),
            template,
            protected: !mpd.ContentProtection.is_empty()
                || !period.ContentProtection.is_empty()
                || !set.ContentProtection.is_empty()
                || !rep.ContentProtection.is_empty(),
        });
    }
    Ok(out)
}

/// How a segment is named: by what its media template is addressed by.
enum Naming {
    Number,
    Time,
    Both,
}

impl Naming {
    fn of(media: &str) -> Self {
        match (media.contains("$Number"), media.contains("$Time")) {
            (true, false) => Naming::Number,
            (false, true) => Naming::Time,
            _ => Naming::Both,
        }
    }

    fn name(&self, number: u64, time: u64) -> String {
        match self {
            Naming::Number => number.to_string(),
            Naming::Time => time.to_string(),
            Naming::Both => format!("{number}_{time}"),
        }
    }

    fn parse(&self, name: &str) -> Result<(u64, u64), String> {
        let num = |s: &str| {
            s.parse::<u64>()
                .map_err(|_| format!("bad segment name {name:?}"))
        };
        match self {
            Naming::Number => Ok((num(name)?, 0)),
            Naming::Time => Ok((0, num(name)?)),
            Naming::Both => {
                let (n, t) = name.split_once('_').ok_or("bad segment name")?;
                Ok((num(n)?, num(t)?))
            }
        }
    }
}

struct Segment {
    number: u64,
    /// Media time, in timescale units.
    time: u64,
    duration: u64,
}

/// The available segments of one track.
fn track_segments(mpd: &MPD, track: &Track) -> Vec<Segment> {
    let live = mpd.mpdtype.as_deref() == Some("dynamic");
    let ast = availability_start(mpd);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let template = &track.template;
    let timescale = template.timescale.unwrap_or(1).max(1) as f64;
    let pto = template.presentationTimeOffset.unwrap_or(0);
    // Media time (in timescale units) of a moment in wall-clock seconds.
    let media_time = |secs: f64| -> Option<u64> {
        let t = (secs - ast? - track.start) * timescale;
        (t >= 0.0).then(|| t as u64 + pto)
    };
    let limit = if live {
        media_time(now)
    } else {
        track
            .end
            .map(|e| ((e - track.start) * timescale) as u64 + pto)
    };
    segments(template, limit, live, mpd, ast, track.start, now)
}

/// The available segments of one period. `limit` is the media time after
/// which no segment is complete (now, or the end of the period).
fn segments(
    template: &SegmentTemplate,
    limit: Option<u64>,
    live: bool,
    mpd: &MPD,
    ast: Option<f64>,
    period_start: f64,
    now: f64,
) -> Vec<Segment> {
    let start_number = template.startNumber.unwrap_or(1);
    let pto = template.presentationTimeOffset.unwrap_or(0);
    let mut out = Vec::new();

    if let Some(timeline) = &template.SegmentTimeline {
        let (mut time, mut number) = (0, start_number);
        for (i, s) in timeline.segments.iter().enumerate() {
            if s.d == 0 {
                continue;
            }
            time = s.t.unwrap_or(time);
            number = s.n.unwrap_or(number);
            let count = match s.r {
                Some(r) if r >= 0 => r as u64 + 1,
                // Repeat until the next S, or for as long as complete segments fit.
                _ if s.r.is_some() => {
                    let end = timeline
                        .segments
                        .get(i + 1)
                        .and_then(|next| next.t)
                        .or(limit);
                    end.map_or(1, |end| end.saturating_sub(time) / s.d)
                }
                _ => 1,
            };
            for _ in 0..count {
                out.push(Segment {
                    number,
                    time,
                    duration: s.d,
                });
                time += s.d;
                number += 1;
            }
        }
        return out;
    }

    // Numbered segments of a fixed duration.
    let Some(duration) = template.duration.filter(|d| *d > 0.0) else {
        return out;
    };
    let timescale = template.timescale.unwrap_or(1).max(1) as f64;
    let seconds = duration / timescale;
    let (first, last) = if live {
        let Some(ast) = ast else { return out };
        let offset = template.availabilityTimeOffset.unwrap_or(0.0);
        let elapsed = now - ast - period_start + offset;
        let complete = (elapsed / seconds).floor();
        if complete < 1.0 {
            return out;
        }
        let window = mpd.timeShiftBufferDepth.map_or(120.0, |d| d.as_secs_f64());
        let count = (window / seconds).floor().max(1.0);
        ((complete - count).max(0.0) as u64, complete as u64)
    } else {
        let end = limit.map_or(0, |l| l.saturating_sub(pto)) as f64;
        (0, (end / duration).ceil() as u64)
    };
    for index in first..last {
        out.push(Segment {
            number: start_number + index,
            time: pto + (index as f64 * duration) as u64,
            duration: duration as u64,
        });
    }
    out
}

fn availability_start(mpd: &MPD) -> Option<f64> {
    mpd.availabilityStartTime
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
}

/// The most common segment duration of the period's best video
/// representation, in seconds.
fn video_grid(period: &Period) -> Option<f64> {
    let (set, rep) = best_video(period)?;
    let template = rendition_template(period, set, rep);
    let timescale = template.timescale.unwrap_or(1).max(1) as f64;
    let mut counts = std::collections::HashMap::new();
    for s in &template.SegmentTimeline?.segments {
        *counts.entry(s.d).or_insert(0) += 1 + s.r.unwrap_or(0).max(0);
    }
    let (d, _) = counts.into_iter().max_by_key(|(d, n)| (*n, *d))?;
    (d > 0).then(|| d as f64 / timescale)
}

/// The most common segment duration.
fn nominal_duration(segments: &[Segment]) -> u64 {
    let mut counts = std::collections::HashMap::new();
    for s in segments {
        *counts.entry(s.duration).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(d, n)| (*n, *d))
        .map_or(1, |(d, _)| d.max(1))
}

/// Start of each period, in seconds from the availability start time.
fn period_starts(mpd: &MPD) -> Vec<f64> {
    let mut starts = Vec::with_capacity(mpd.periods.len());
    let mut next = 0.0;
    for period in &mpd.periods {
        let start = period.start.map_or(next, |s| s.as_secs_f64());
        next = start + period.duration.map_or(0.0, |d| d.as_secs_f64());
        starts.push(start);
    }
    starts
}

fn audio_sets(period: &Period) -> Vec<&AdaptationSet> {
    period
        .adaptations
        .iter()
        .filter(is_audio_adaptation)
        .collect()
}

fn best_video(period: &Period) -> Option<(&AdaptationSet, &Representation)> {
    period
        .adaptations
        .iter()
        .filter(is_video_adaptation)
        .flat_map(|set| set.representations.iter().map(move |rep| (set, rep)))
        .max_by_key(|(_, rep)| rep.bandwidth.unwrap_or(0))
}

/// The adaptation set and representation of `rendition` in `period`. `v`
/// is the best video; `a<i>` is the i-th audio set of the last period
/// (`audio_ref`), found again in other periods by language: ad periods carry
/// their own sets.
fn find_rendition<'a>(
    period: &'a Period,
    rendition: &str,
    audio_ref: &[&AdaptationSet],
) -> Option<(&'a AdaptationSet, &'a Representation)> {
    if rendition == "v" {
        return best_video(period);
    }
    let index: usize = rendition.strip_prefix('a')?.parse().ok()?;
    let lang = |set: &AdaptationSet| set.lang.clone().unwrap_or_default();
    let wanted = lang(audio_ref.get(index)?);
    // Its position among the sets of the same language.
    let nth = audio_ref[..index]
        .iter()
        .filter(|set| lang(set) == wanted)
        .count();
    let sets = audio_sets(period);
    let same_lang: Vec<&AdaptationSet> = sets
        .iter()
        .copied()
        .filter(|set| lang(set) == wanted)
        .collect();
    let set = same_lang.get(nth).or(same_lang.first()).or(sets.first())?;
    best(set).map(|rep| (*set, rep))
}

fn best(set: &AdaptationSet) -> Option<&Representation> {
    set.representations
        .iter()
        .max_by_key(|rep| rep.bandwidth.unwrap_or(0))
}

fn codecs(set: &AdaptationSet, rep: &Representation) -> Option<String> {
    rep.codecs.clone().or_else(|| set.codecs.clone())
}

/// `25`, `50/1` or `30000/1001` as frames per second.
fn frame_rate(value: &str) -> Option<f64> {
    match value.split_once('/') {
        Some((n, d)) => Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?),
        None => value.parse().ok(),
    }
}

fn join(base: &Url, urls: &[BaseURL]) -> Url {
    urls.first()
        .map(|b| b.base.trim())
        .filter(|b| !b.is_empty())
        .and_then(|b| base.join(b).ok())
        .unwrap_or_else(|| base.clone())
}

/// The segment template in effect for a representation.
fn rendition_template(
    period: &Period,
    set: &AdaptationSet,
    rep: &Representation,
) -> SegmentTemplate {
    let period_level = merge(&SegmentTemplate::default(), period.SegmentTemplate.as_ref());
    merge(
        &merge(&period_level, set.SegmentTemplate.as_ref()),
        rep.SegmentTemplate.as_ref(),
    )
}

/// `own` over `parent`, attribute by attribute.
fn merge(parent: &SegmentTemplate, own: Option<&SegmentTemplate>) -> SegmentTemplate {
    let Some(own) = own else {
        return parent.clone();
    };
    SegmentTemplate {
        media: own.media.clone().or_else(|| parent.media.clone()),
        initialization: own
            .initialization
            .clone()
            .or_else(|| parent.initialization.clone()),
        startNumber: own.startNumber.or(parent.startNumber),
        duration: own.duration.or(parent.duration),
        timescale: own.timescale.or(parent.timescale),
        presentationTimeOffset: own.presentationTimeOffset.or(parent.presentationTimeOffset),
        availabilityTimeOffset: own.availabilityTimeOffset.or(parent.availabilityTimeOffset),
        SegmentTimeline: own
            .SegmentTimeline
            .clone()
            .or_else(|| parent.SegmentTimeline.clone()),
        ..Default::default()
    }
}

/// `{prefix}/hls/[{key}/]m={manifest}/{rendition}/index.m3u8`.
fn playlist_url(params: &HlsParams, rendition: &str) -> String {
    let manifest = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(params.source_url);
    let key = params.key.map(|k| format!("{k}/")).unwrap_or_default();
    format!(
        "{}/hls/{key}m={manifest}/{rendition}/index.m3u8",
        params.prefix
    )
}
