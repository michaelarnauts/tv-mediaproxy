# tv-mediaproxy — DASH ClearKey passthrough

A cut-down fork of [mediaflow-proxy-light](https://github.com/mhdzumair/mediaflow-proxy-light)
that does one thing: play ClearKey-protected MPEG-DASH streams in players that cannot decrypt
them themselves (Jellyfin, VLC, …).

Unlike upstream it does **not** convert DASH to HLS. The manifest stays a DASH manifest, with all
its periods and the origin's whole time-shift window, so a player can seek back as far as the
origin allows. Only addressing and protection change; segments are decrypted as they are
requested.

Everything else upstream ships (HLS proxy, DASH→HLS, extractors, Xtream, Telegram, Acestream,
transcoding, EPG, web UI, speedtest, mobile builds) has been removed.

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
| `mpd` | fetches the manifest and rewrites it, see below. Without keys only the addressing is rewritten. |
| `seg` | fetches an init or media segment of protected content (the rewritten manifest builds these URLs). One that matches the media template in `t=` is a media segment and is decrypted; anything else is the init segment and loses its protection boxes. |

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
`ghcr.io/michaelarnauts/tv-mediaproxy` (`:latest` follows `main`, a `v*` tag publishes
`:<version>` and a release with the binary).

The image only wraps a static binary built beforehand:

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
