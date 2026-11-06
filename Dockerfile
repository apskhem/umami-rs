# syntax=docker/dockerfile:1
FROM rust:1.99-slim-bookworm AS builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p umami-server \
    && cp target/release/umami-server /usr/local/bin/umami-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/umami-server /usr/local/bin/umami-server
# Same layout as upstream's image: a GeoLite2 database mounted at /app/geo is picked up.
WORKDIR /app
ENV PORT=3000
EXPOSE 3000
USER nobody
HEALTHCHECK --interval=5s --timeout=3s --retries=12 \
    CMD curl -fsS "http://localhost:${PORT}/api/heartbeat" || exit 1
CMD ["umami-server"]
