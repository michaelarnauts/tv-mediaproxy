//! MPEG-TS out of a DASH manifest: GPAC plays the manifest (from the live
//! edge, top quality, ClearKey decryption) and its stdout is the response.
//!
//! One `gpac` process per request; it is killed when the client goes away, or
//! when it stops producing output (a stalled stream also hides a client that
//! went away: nothing is written, so the disconnect goes unnoticed).

use std::process::Stdio;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::Stream;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, Command};

use crate::error::{AppError, AppResult};

/// How long GPAC may take to produce its first bytes.
const FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a running stream may go without output. Live GPAC output comes
/// at least once per segment.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 64 * 1024;

/// Start `gpac` on `url` and return its MPEG-TS output. Waits for the first
/// bytes, so a stream that cannot start is an error rather than an empty 200.
/// `spd_ms` is added to the manifest's suggestedPresentationDelay, and can be
/// negative.
pub async fn play(
    gpac: &str,
    url: &str,
    keys: Option<&str>,
    spd_ms: i64,
) -> AppResult<impl Stream<Item = std::io::Result<Bytes>>> {
    let mut child = Command::new(gpac)
        .args(gpac_args(url, keys, spd_ms))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AppError::Internal(format!("Cannot start {gpac}: {e}")))?;
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

/// The `gpac` command line playing `url` into MPEG-TS on stdout.
fn gpac_args(url: &str, keys: Option<&str>, spd_ms: i64) -> Vec<String> {
    let mut args = vec![
        "-logs=all@warning:ncl".into(),
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
