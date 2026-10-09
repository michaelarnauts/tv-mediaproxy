# The image wraps a static (musl) binary built beforehand:
#
#   cargo build --release --target x86_64-unknown-linux-musl
#   mkdir -p dist && cp target/x86_64-unknown-linux-musl/release/tv-mediaproxy dist/
#   docker build .
#
# CI does the same with the binary from its build job.

FROM gcr.io/distroless/static-debian12
WORKDIR /app
COPY dist/tv-mediaproxy /app/tv-mediaproxy

# Configure with environment variables (API_PASSWORD, APP__SERVER__PORT, ...)
# or mount a config file and point CONFIG_PATH at it.
ENV RUST_LOG=info
ENV APP__SERVER__HOST=0.0.0.0
EXPOSE 8888
ENTRYPOINT ["/app/tv-mediaproxy"]
