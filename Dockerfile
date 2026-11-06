# syntax=docker/dockerfile:1
FROM rust:1.99-alpine3.23 AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p umami-server \
    && cp target/release/umami-server /usr/local/bin/umami-server

# The binary is fully static (musl) and carries its own TLS root certificates, so it
# needs nothing else: no shell, libc or CA bundle.
FROM scratch
COPY --from=builder /usr/local/bin/umami-server /usr/local/bin/umami-server
# Same layout as upstream's image: a GeoLite2 database mounted at /app/geo is picked up.
WORKDIR /app
ENV PORT=3000
EXPOSE 3000
# nobody:nogroup (there is no /etc/passwd to look the name up in).
USER 65534:65534
HEALTHCHECK --interval=5s --timeout=3s --retries=12 \
    CMD ["/usr/local/bin/umami-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/umami-server"]
