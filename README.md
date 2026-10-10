# tv-mediaproxy — DASH ClearKey passthrough

A cut-down fork of [mediaflow-proxy-light](https://github.com/mhdzumair/mediaflow-proxy-light)
that does one thing: play ClearKey-protected MPEG-DASH streams in players that cannot decrypt
them themselves. It serves them two ways:

- as **DASH** (`mpd`), for DASH players without ClearKey support,
- as one continuous **MPEG-TS** stream (`ts`), played by [GPAC](https://gpac.io) on the server,
  for players and servers that are better at TS than at live DASH (Jellyfin, VLC, …).

Unlike upstream it does **not** convert DASH to HLS. The manifest stays a DASH manifest, with all
its periods and the origin's whole time-shift window, so a player can seek back as far as the
origin allows. Only addressing and protection change; segments are decrypted as they are
requested.

Everything else upstream ships (HLS proxy, DASH→HLS, extractors, Xtream, Telegram, Acestream,
transcoding, EPG, web UI, speedtest, mobile builds) has been removed.

This fork lives on the `tv` branch; `main` mirrors upstream.

## Usage

Put the proxy in front of the playlist URL:

```
http://<host>:8888/<password>/m3u/https://tv.example/iptv.m3u
```

`<password>` is `API_PASSWORD`; the proxy refuses to start without one. `/health` and `/metrics`
are open.

Every path is `/<password>/<kind>/[<kid>:<key>[,…]/][t=<templates>/]<upstream URL>`, where `kind`
says what the URL is:

| Kind | The proxy |
|---|---|
| `m3u` | fetches the playlist and routes every MPD entry with `#KODIPROP:inputstream.adaptive.license_type=org.w3.clearkey` and `license_key=<kid>:<key>[,…]` through itself as `/<password>/mpd/<kid>:<key>/<mpd url>`. Those entries lose their `inputstream.adaptive.*` KODIPROPs (the result is no longer encrypted); everything else is passed through untouched. |
| `m3u-ts` | the same, but routes those entries to `ts`, and unprotected MPD entries too (as `/<password>/ts/<mpd url>`). |
| `mpd` | fetches the manifest and rewrites it, see below. Without keys only the addressing is rewritten. |
| `seg` | fetches an init or media segment of protected content (the rewritten manifest builds these URLs). One that matches the media template in `t=` is a media segment and is decrypted; anything else is the init segment and loses its protection boxes. |
| `ts` | plays the manifest with GPAC and streams it as MPEG-TS (`video/mp2t`), see below. The keys are optional. |

The upstream URL is used as written, query string included, so a catch-up parameter appended by a
player reaches the origin. `https:/` with one slash is accepted too, for clients and proxies that
collapse `//`. The access log shows the path with password, keys and templates replaced by `…`.

## How the manifest is rewritten

- Every `BaseURL` is resolved against the manifest URL *after redirects* and removed. Each element
  that owns a segment template gets one absolute `BaseURL` instead. (Players resolve a template
  from the element that defines it; VLC ignores a `BaseURL` below that.)
- For protected content that `BaseURL` points into this proxy:
  `/<password>/seg/<kid>:<key>/t=<templates>/<origin base>`. The segment templates themselves are left
  untouched, so the URLs the player builds carry the keys and `t=`: the absolute initialization and
  media templates, base64url. To decrypt a media segment the proxy matches its URL against the
  media template, fills the representation into the initialization template and fetches (and
  caches) that init segment. No state is kept between requests.
- Clear content (subtitles, unencrypted ad periods) keeps pointing at the origin.
- `ContentProtection`, `Location` and `PatchLocation` are dropped.
- `startNumber` is dropped from templates addressed by `$Time$` — a player workaround, see below.

Everything else — timelines, periods, `availabilityStartTime`, `timeShiftBufferDepth`, events — is
written back unchanged.

Only `SegmentTemplate` addressing is rewritten. A representation using `SegmentBase` or
`SegmentList` keeps an absolute origin `BaseURL`, so it only plays if it is not encrypted.

Decryption supports `cenc` (AES-CTR) and, from upstream, `cbcs`/`cbc1`/`cens`; only `cenc` has
been tested against live streams.

## MPEG-TS through GPAC

For `ts` the proxy starts one `gpac` process per request and streams its stdout; the process is
killed when the client disconnects. GPAC is a full DASH client: it follows manifest updates and
period changes, picks the top quality, decrypts with the keys from the URL and muxes the
streams into MPEG-TS. The response starts once GPAC has produced its first bytes
(typically under a second); a stream that cannot start is a 502.

It starts `ts.start_buffer` seconds (default 12) before the newest segment the manifest lists.
Those segments are already published, so the player — or a media server's ffmpeg, which wants a
few seconds before it cuts its first HLS segment — gets them at once and starts with a full
buffer; starting at the live point, it would get them one by one as they appear. The start is
measured from the manifest rather than the clock because origins publish late, each by its own
amount (from ~0 to ~30 s), whatever `suggestedPresentationDelay` they announce. (GPAC adds its
`spd` option to the manifest's `suggestedPresentationDelay`, so the proxy passes the
difference.)

GPAC is built from a pinned commit with the patches in [`gpac/`](gpac/) (see the `Dockerfile`):

| Patch | Fixes |
|---|---|
| `0001` | `cdcrypt` ignored keys given as `kids`/`keys` options when the init segment has a `pssh`, and output garbage. |
| `0002` | A manifest written as `<MPD` followed by a newline was not recognised as DASH; another demuxer took it. |
| `0003` | MPD updates from a server that always sends `Content-Encoding: gzip` failed to parse (GPAC's downloader does not decode it). |
| `0004` | The presentation delay also delayed segment availability, so GPAC waited up to a segment duration for a segment that was long published. |

They should go upstream; drop each one once GPAC has its fix. Two GPAC limits remain: its
downloader ignores `Content-Encoding` (the proxy passes the manifest type explicitly so GPAC
does not have to detect it), and its option parser breaks on a URL with `://` inside its path.

## Player workarounds

These change the manifest for every player, not only the one that needs it.

**`startNumber` on `$Time$` templates is dropped**, for ffmpeg up to 8.1 — the DASH demuxer
Jellyfin uses. With a `startNumber` on a live `SegmentTimeline`, it reads the same segment again
after every manifest reload: timestamps jump back, the stream crawls or stalls. With `$Time$`
addressing the number is not part of any segment URL, so other players lose nothing. FFmpeg 9.0 has the fix (jellyfin-ffmpeg 8.1.3 does not).
**Remove this once Jellyfin ships ffmpeg 9**: it is `merge_template` in `src/mpd/rewrite.rs`.

Not a workaround, but a choice made for VLC: the proxy `BaseURL` goes on the element that defines
the segment template rather than on each representation, because VLC 3 ignores a `BaseURL` below
that element. Placed there it is valid DASH for every player.

## Running

```bash
cargo build --release
API_PASSWORD=secret APP__SERVER__HOST=0.0.0.0 ./target/release/tv-mediaproxy
# then point a player at
#   http://<host>:8888/secret/m3u/https://tv.example/iptv.m3u
```

Or download the static Linux x86-64 binary from a release, or run the Docker image
`ghcr.io/michaelarnauts/tv-mediaproxy` (`:dev-tv` follows the `tv` branch, a `v*` tag publishes
`:<version>` and a release with the binary). The binary alone serves everything but `ts`, which
needs the patched `gpac` (path in `ts.gpac`, default `gpac` on the `PATH`).

The image builds the patched GPAC itself and wraps a static binary built beforehand:

```bash
cargo build --release --target x86_64-unknown-linux-musl   # needs musl-tools
mkdir -p dist && cp target/x86_64-unknown-linux-musl/release/tv-mediaproxy dist/
docker build -t tv-mediaproxy .
```

Configuration is by environment (`APP__SECTION__KEY`, plus upstream's `API_PASSWORD`, `PORT`,
`PROXY_URL`, …) or a TOML file named by `CONFIG_PATH`; see `config-example.toml`.

URLs this proxy generates are built from the request's `Host` (and `X-Forwarded-*` headers), so
fetch the playlist through the address the player will use.

## License

MIT, as upstream.
