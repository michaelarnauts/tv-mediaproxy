# tv-mediaproxy

A transparent proxy that decrypts ClearKey-encrypted MPEG-DASH streams, so players that cannot
decrypt them can play them. Prefix the URL of an M3U playlist whose DASH streams carry their
ClearKey keys (`#KODIPROP:inputstream.adaptive.license_key=<kid>:<key>`) with the proxy's URL.

## Features

- **DASH passthrough** — the manifest stays DASH, its segments are decrypted as they are
  requested (`cenc` and `cbcs`), and you can still seek back as far as the origin allows
- **MPEG-TS output** — every stream as one continuous MPEG-TS stream, played by
  [GPAC](https://gpac.io), for players that are better at TS than at live DASH
- **HDHomeRun tuner** — the playlist's channels as a tuner for Plex and Jellyfin Live TV, with a
  matching XMLTV guide

---

## Installation

### Docker

```bash
docker run -d \
  --name tv-mediaproxy \
  -p 8888:8888 \
  ghcr.io/michaelarnauts/tv-mediaproxy:latest
```

The image is `linux/amd64` only.

### From source

```bash
cargo build --release
./target/release/tv-mediaproxy
```

`gpac` must be on the `PATH`, built from master at
[3f05654](https://github.com/gpac/gpac/commit/3f056542b377cc73677d19490bfe1b46c52c9be7) or later,
with the patches in `gpac/` (see Workarounds).

## Configuration

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8888` | Port the proxy listens on. |
| `TS_START_BUFFER` | `12` | Seconds behind the newest segment that `ts` starts. Those segments are already published, so a client gets them at once: Jellyfin and Plex (ffmpeg) want a few seconds of media before they start, and would otherwise wait for them to appear one by one. |
| `HDHR_TUNERS` | `6` | Tuners `hdhr` announces: how many channels a client may play at once. |
| `RUST_LOG` | `info` | Log filter. |

---

## Usage

Give your player one of these:

| URL | For |
|---|---|
| `http://<host>:8888/m3u/<playlist url>` | DASH players. The streams stay DASH, decrypted. |
| `http://<host>:8888/m3u-ts/<playlist url>` | Players that are better at MPEG-TS than at live DASH, like VLC. |
| `http://<host>:8888/hdhr/m3u/<playlist url>` | Plex and Jellyfin Live TV: an HDHomeRun tuner with the playlist's channels as MPEG-TS. |
| `http://<host>:8888/hdhr/guide/<playlist url>` | The guide for that tuner: the playlist's guide, limited to those channels. |

The playlist URL is taken as written, query included.

```bash
# A DASH player
mpv "http://localhost:8888/m3u/https://example.com/iptv.m3u"

# VLC, as MPEG-TS
vlc "http://localhost:8888/m3u-ts/https://example.com/iptv.m3u"

# Jellyfin: Live TV → Tuner devices → HDHomeRun, and TV guide data providers → XMLTV
http://192.168.1.10:8888/hdhr/m3u/https://example.com/iptv.m3u
http://192.168.1.10:8888/hdhr/guide/https://example.com/iptv.m3u
```

## Routes

The playlist comes back with its streams pointing at the proxy:

| Path | Description |
|---|---|
| `/mpd/<keys>/<manifest>` | The manifest, rewritten so the segments of protected streams also go through the proxy (via `seg`), without its encryption. Everything else stays as it is. |
| `/seg/<keys>/t=<templates>/<segment>` | A segment, decrypted. `t=` carries the init and media templates, so a media segment leads back to its init segment. |
| `/ts/<keys>/<manifest>` | The stream played by GPAC, one process per viewer, starting `TS_START_BUFFER` seconds behind the newest segment. |

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
| `0009` | [#4041](https://github.com/gpac/gpac/pull/4041) | a crash on AdaptationSets linked by `adaptation-set-switching` when playback starts outside the first Period |

---

## Development

`rust-toolchain.toml` pins the Rust version, for local builds, the image and CI alike.

```bash
cargo build --release
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo machete            # unused dependencies
```

---

## License

[MIT License](LICENSE)

## Credits

- [mediaflow-proxy-light](https://github.com/mhdzumair/mediaflow-proxy-light) by Mohamed Zumair
