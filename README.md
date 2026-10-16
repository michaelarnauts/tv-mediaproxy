# tv-mediaproxy

Plays ClearKey-protected MPEG-DASH in players that cannot decrypt it. Put the proxy in front of
an M3U playlist whose DASH streams carry their ClearKey keys
(`#KODIPROP:inputstream.adaptive.license_key=<kid>:<key>`).

## Running

### Docker

```bash
docker run -p 8888:8888 ghcr.io/michaelarnauts/tv-mediaproxy
```

The image brings the patched GPAC (see Workarounds).

### From source

```bash
cargo build --release
./target/release/tv-mediaproxy
```

`ts` (and so `m3u-ts` and `hdhr`) needs the patched `gpac` on the `PATH`; the rest works without.

### Environment

| Variable | Default | |
|---|---|---|
| `PORT` | `8888` | Port the proxy listens on. |
| `TS_START_BUFFER` | `12` | Seconds behind the newest segment that `ts` starts. Those segments are already published, so a client gets them at once: Jellyfin and Plex (ffmpeg) want a few seconds of media before they start, and would otherwise wait for them to appear one by one. |
| `HDHR_TUNERS` | `6` | Tuners `hdhr` announces: how many channels a client may play at once. |
| `RUST_LOG` | `info` | Log filter. |

## Usage

Give your player one of these:

| URL | For |
|---|---|
| `http://<host>:8888/m3u/<playlist url>` | DASH players. The streams stay DASH, decrypted, and you can still seek back as far as the origin allows. |
| `http://<host>:8888/m3u-ts/<playlist url>` | Players that are better at MPEG-TS than at live DASH, like VLC. Every stream becomes one continuous MPEG-TS stream. |
| `http://<host>:8888/hdhr/m3u/<playlist url>` | Plex and Jellyfin Live TV: an HDHomeRun tuner with the playlist's channels as MPEG-TS. Use `…/hdhr/guide/<playlist url>` as its guide: the playlist's guide, limited to those channels. |

For example `http://192.168.1.10:8888/m3u-ts/https://tv.example/iptv.m3u`. The playlist URL is
taken as written, query included.

## How it works

The playlist comes back with its streams pointing at the proxy:

| Route | Serves |
|---|---|
| `/mpd/<keys>/<manifest>` | the manifest, rewritten so the segments of protected streams also go through the proxy (via `seg`), without its encryption. Everything else stays as it is. Supports `cenc` and `cbcs`. |
| `/seg/<keys>/t=<templates>/<segment>` | a segment, decrypted. `t=` carries the init and media templates, so a media segment leads back to its init segment. |
| `/ts/<keys>/<manifest>` | the stream played by [GPAC](https://gpac.io), one process per viewer, starting `TS_START_BUFFER` seconds behind the newest segment. |

`<keys>` is `<kid>:<key>[,…]`, or `-` for a clear stream. These URLs carry the upstream URL as
`m=<base64url>`, so no client can rewrite its query string (Plex does), and use the address the
playlist was fetched from.

## Workarounds

- **ffmpeg up to 8.1** (Jellyfin) repeats segments of a live `$Time$` timeline that has a
  `startNumber`, so `mpd` drops it. Fixed in ffmpeg 9.0 by
  [8a8bde6](https://github.com/FFmpeg/FFmpeg/commit/8a8bde6a54e282fe42a9cf9467c4af9c5fe009ab);
  remove the workaround once Jellyfin ships it.
- **VLC** ignores a `BaseURL` below the element that defines the segment template, so `mpd` puts
  the `BaseURL` on that element.
- **CDNs** may refuse clients that are not a browser, so the proxy and GPAC send a Chrome
  User-Agent.
- **GPAC** is built with these patches; drop one once GPAC merges its PR:

| Patch | PR | Fixes |
|---|---|---|
| `0001` | [#3992](https://github.com/gpac/gpac/pull/3992) | `cdcrypt` ignored `kids`/`keys` for MP4 input |
| `0002` | [#3991](https://github.com/gpac/gpac/pull/3991) | `<MPD` followed by a newline was not probed as DASH |
| `0003` | [#3993](https://github.com/gpac/gpac/pull/3993) | gzip-encoded manifest updates failed to parse |
| `0004` | [#3995](https://github.com/gpac/gpac/pull/3995) | the presentation delay also delayed segment availability |
| `0005` | [#3990](https://github.com/gpac/gpac/pull/3990) | fractional seconds in durations were read as milliseconds |
| `0006` | [#3996](https://github.com/gpac/gpac/pull/3996) | an HTTP/2 manifest update could be lost, ending the stream |
| `0007` | [#3994](https://github.com/gpac/gpac/pull/3994) | the segment position moved once per quality on each update |
| `0008` | [#3997](https://github.com/gpac/gpac/pull/3997) | `-user-agent`/`-ua` never reached HTTP requests |

## Credits

- [mediaflow-proxy-light](https://github.com/mhdzumair/mediaflow-proxy-light) by Mohamed Zumair
