# The image wraps a static (musl) binary built beforehand, plus GPAC for /ts/:
#
#   cargo build --release --target x86_64-unknown-linux-musl
#   mkdir -p dist && cp target/x86_64-unknown-linux-musl/release/tv-mediaproxy dist/
#   docker build .
#
# CI does the same with the binary from its build job.

# GPAC at a pinned commit with the fixes in gpac/*.patch, linked into one
# binary (no ffmpeg, no display or audio output).
FROM debian:trixie-slim AS gpac
ARG GPAC_COMMIT=6a8ebb71b7e19d47023e4f91ee960f3636644403
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config git ca-certificates \
      zlib1g-dev libssl-dev libnghttp2-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src/gpac
RUN git init -q \
 && git fetch -q --depth 1 https://github.com/gpac/gpac.git "$GPAC_COMMIT" \
 && git checkout -q FETCH_HEAD
COPY gpac/*.patch /src/
RUN git apply /src/*.patch \
 && ./configure --static-build --disable-x11 \
 && make -j"$(nproc)" \
 && install -m 755 bin/gcc/gpac /usr/local/bin/gpac

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3t64 libnghttp2-14 zlib1g \
 && rm -rf /var/lib/apt/lists/*
COPY --from=gpac /usr/local/bin/gpac /usr/local/bin/gpac
# GPAC builds its option registry in ~/.gpac on its first run; do that now
# rather than on the first stream. creds.key is per installation: not baked in.
RUN gpac -h > /dev/null 2>&1; rm -f /root/.gpac/creds.key; test -f /root/.gpac/GPAC.cfg
WORKDIR /app
COPY dist/tv-mediaproxy /app/tv-mediaproxy

# Configure with environment variables (API_PASSWORD, APP__SERVER__PORT, ...)
# or mount a config file and point CONFIG_PATH at it.
ENV RUST_LOG=info
ENV APP__SERVER__HOST=0.0.0.0
EXPOSE 8888
ENTRYPOINT ["/app/tv-mediaproxy"]
