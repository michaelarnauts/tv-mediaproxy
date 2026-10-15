//! MPEG-TS out of a DASH manifest: GPAC plays the manifest (top quality,
//! ClearKey decryption) and its stdout is the response.
//!
//! One `gpac` process per request; it is killed when the client goes away, or
//! when it stops producing output (a stalled stream also hides a client that
//! went away: nothing is written, so the disconnect goes unnoticed).

use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use dash_mpd_core::{is_audio_adaptation, is_video_adaptation, SegmentTemplate, MPD};
use futures::Stream;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, Command};

use crate::error::{AppError, AppResult};
use crate::handler::fetch;

/// How long GPAC may take to produce its first bytes.
const FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a running stream may go without output. Live GPAC output comes
/// at least once per segment.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 64 * 1024;

/// Start `gpac` on `url` and return its MPEG-TS output. Waits for the first
/// bytes, so a stream that cannot start is an error rather than an empty 200.
pub async fn play(
    client: &reqwest::Client,
    url: &str,
    keys: Option<&str>,
) -> AppResult<impl Stream<Item = std::io::Result<Bytes>>> {
    // Start TS_START_BUFFER seconds before the newest published segment, so
    // the player fills its buffer at once. Origins publish late by different
    // amounts, so this is measured from the manifest.
    let manifest = fetch(client, url).await?;
    let mpd = dash_mpd_core::parse(&String::from_utf8_lossy(&manifest.body))
        .map_err(|e| AppError::Upstream(format!("MPD from {}: {e}", manifest.url)))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    // GPAC fetches a segment only once the clock has passed its end, so
    // segments listed ahead of the clock do not count.
    let behind = live_edge(&mpd, now).map_or(0.0, |edge| (now - edge).max(0.0));
    let delay_ms = ((behind + crate::env("TS_START_BUFFER", 12.0)) * 1000.0) as i64;
    tracing::info!(
        "ts: newest segment {behind:.1}s behind live, starting {:.1}s behind",
        delay_ms as f64 / 1000.0
    );
    // GPAC adds its spd to the manifest's suggestedPresentationDelay.
    let suggested_ms = mpd
        .suggestedPresentationDelay
        .map_or(0, |d| d.as_millis() as i64);

    let mut child = Command::new("gpac")
        .args(gpac_args(url, keys, delay_ms - suggested_ms))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AppError::Internal(format!("Cannot start gpac: {e}")))?;
    tokio::spawn(log_stderr(child.stderr.take().expect("stderr is piped")));
    let mut stdout = child.stdout.take().expect("stdout is piped");

    let mut first = BytesMut::with_capacity(CHUNK);
    let read = tokio::time::timeout(FIRST_DATA_TIMEOUT, stdout.read_buf(&mut first))
        .await
        .map_err(|_| {
            AppError::Upstream(format!(
                "No MPEG-TS from GPAC within {FIRST_DATA_TIMEOUT:?}"
            ))
        })?
        .map_err(|e| AppError::Internal(format!("Reading GPAC output: {e}")))?;
    if read == 0 {
        let status = child
            .wait()
            .await
            .map(|s| s.to_string())
            .unwrap_or_default();
        return Err(AppError::Upstream(format!(
            "GPAC ended without output ({status})"
        )));
    }

    // The child rides along in the stream state: dropping the response drops it,
    // which kills the process.
    let state = (child, stdout, Some(first.freeze()));
    Ok(futures::stream::unfold(
        state,
        |(child, mut stdout, first)| async move {
            if let Some(first) = first {
                return Some((Ok(first), (child, stdout, None)));
            }
            let mut buf = BytesMut::with_capacity(CHUNK);
            let Ok(read) = tokio::time::timeout(IDLE_TIMEOUT, stdout.read_buf(&mut buf)).await
            else {
                tracing::warn!("No GPAC output for {IDLE_TIMEOUT:?}, ending the stream");
                return None;
            };
            match read {
                Ok(0) => None,
                Ok(_) => Some((Ok(buf.freeze()), (child, stdout, None))),
                Err(e) => Some((Err(e), (child, stdout, None))),
            }
        },
    ))
}

/// When the newest segment of the last period ends, in seconds since the
/// epoch: of the best video representation, else of the first audio one.
fn live_edge(mpd: &MPD, now: f64) -> Option<f64> {
    let ast = mpd.availabilityStartTime?.timestamp_millis() as f64 / 1000.0;
    let (mut start, mut next) = (0.0, 0.0);
    for period in &mpd.periods {
        start = period.start.map_or(next, |s| s.as_secs_f64());
        next = start + period.duration.map_or(0.0, |d| d.as_secs_f64());
    }
    let period = mpd.periods.last()?;
    let sets = &period.adaptations;
    let set = (sets.iter().find(is_video_adaptation))
        .or_else(|| sets.iter().find(is_audio_adaptation))?;
    let rep = set.representations.iter().max_by_key(|r| r.bandwidth)?;
    // The template in effect, attribute by attribute.
    let levels = [
        &rep.SegmentTemplate,
        &set.SegmentTemplate,
        &period.SegmentTemplate,
    ];
    fn pick<T>(
        levels: &[&Option<SegmentTemplate>],
        f: impl Fn(&SegmentTemplate) -> Option<T>,
    ) -> Option<T> {
        levels.iter().filter_map(|t| t.as_ref()).find_map(f)
    }
    let timescale = pick(&levels, |t| t.timescale).unwrap_or(1).max(1) as f64;
    let pto = pick(&levels, |t| t.presentationTimeOffset).unwrap_or(0);
    let elapsed = now - ast - start;

    let end = match pick(&levels, |t| t.SegmentTimeline.clone()) {
        Some(timeline) => {
            let limit = pto + (elapsed * timescale).max(0.0) as u64;
            let mut time = 0;
            for (i, s) in timeline.segments.iter().enumerate() {
                time = s.t.unwrap_or(time);
                let count = match s.r {
                    Some(r) if r >= 0 => r as u64 + 1,
                    // Repeat until the next S, or for as long as segments fit.
                    Some(_) => {
                        let end = timeline.segments.get(i + 1).and_then(|n| n.t);
                        end.unwrap_or(limit).saturating_sub(time) / s.d.max(1)
                    }
                    None => 1,
                };
                time += count * s.d;
            }
            time.saturating_sub(pto) as f64 / timescale
        }
        None => {
            let duration = pick(&levels, |t| t.duration)? / timescale;
            let offset = pick(&levels, |t| t.availabilityTimeOffset).unwrap_or(0.0);
            ((elapsed + offset) / duration).floor() * duration
        }
    };
    Some(ast + start + end)
}

/// The `gpac` command line playing `url` into MPEG-TS on stdout.
fn gpac_args(url: &str, keys: Option<&str>, spd_ms: i64) -> Vec<String> {
    let mut args = vec![
        "-logs=all@warning:ncl".into(),
        format!("-ua={}", crate::USER_AGENT),
        "-i".into(),
        // `:gpac:` ends the URL for GPAC's option parser. The mime type spares
        // probing, which fails on gzip-encoded manifests. GPAC starts the
        // manifest's suggestedPresentationDelay plus `spd` ms behind live (by
        // default it ignores the manifest's).
        format!("{url}:gpac:mime=application/dash+xml:algo=none:start_with=max_bw:spd={spd_ms}"),
    ];
    if let Some(keys) = keys {
        let (kids, keys): (Vec<_>, Vec<_>) = keys
            .split(',')
            .filter_map(|pair| pair.trim().split_once(':'))
            .map(|(kid, key)| (format!("0x{kid}"), format!("0x{key}")))
            .unzip();
        args.push(format!(
            "cdcrypt:kids={}:keys={}",
            kids.join(","),
            keys.join(",")
        ));
    }
    // One AAC frame per PES: packing several garbles 44.1 kHz audio timestamps.
    args.extend(["-o".into(), "stdout:ext=ts:pes_pack=none".into()]);
    args
}

async fn log_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::warn!("gpac: {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_command_line_with_keys() {
        let args = gpac_args("https://cdn/a.mpd?x=1", Some("aa:bb,cc:dd"), -4000);
        assert_eq!(
            args,
            [
                "-logs=all@warning:ncl",
                &format!("-ua={}", crate::USER_AGENT),
                "-i",
                "https://cdn/a.mpd?x=1:gpac:mime=application/dash+xml:algo=none:start_with=max_bw:spd=-4000",
                "cdcrypt:kids=0xaa,0xcc:keys=0xbb,0xdd",
                "-o",
                "stdout:ext=ts:pes_pack=none",
            ]
        );
        assert!(!gpac_args("https://cdn/a.mpd", None, 0)
            .iter()
            .any(|a| a.starts_with("cdcrypt")));
    }
}
