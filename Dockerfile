# GPAC at a pinned commit, linked into one
# binary (no ffmpeg, no display or audio output).
FROM debian:trixie-slim AS gpac
ARG GPAC_COMMIT=7cda8f465456c730a8cea78305b9b95ebb9b63fa
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config git ca-certificates \
      zlib1g-dev libssl-dev libnghttp2-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src/gpac
RUN git init -q \
 && git fetch -q --depth 1 https://github.com/gpac/gpac.git "$GPAC_COMMIT" \
 && git checkout -q FETCH_HEAD
RUN ./configure --static-build --disable-x11 \
 && make -j"$(nproc)" \
 && install -m 755 bin/gcc/gpac /usr/local/bin/gpac

# rustup installs the toolchain rust-toolchain.toml pins.
FROM rust:slim-trixie AS proxy
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3t64 libnghttp2-14 zlib1g \
 && rm -rf /var/lib/apt/lists/*
COPY --from=gpac /usr/local/bin/gpac /usr/local/bin/gpac
# GPAC builds its option registry in ~/.gpac on its first run; do that now
# rather than on the first stream. creds.key is per installation: not baked in.
RUN gpac -h > /dev/null 2>&1; rm -f /root/.gpac/creds.key; test -f /root/.gpac/GPAC.cfg
COPY --from=proxy /src/target/release/tv-mediaproxy /usr/local/bin/tv-mediaproxy
EXPOSE 8888
ENTRYPOINT ["tv-mediaproxy"]
