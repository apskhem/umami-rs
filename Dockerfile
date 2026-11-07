# syntax=docker/dockerfile:1

# The dashboard: upstream's UI built from its source as static files (see
# tools/dashboard/prepare.mts). BASE_PATH is baked into the build, as with
# upstream's image. To export just the files:
#
#   docker build --target dashboard --output type=local,dest=dashboard .
FROM node:22.23.4-alpine AS dashboard-build
ARG UMAMI_VERSION="3.4.1"
ARG PNPM_VERSION="12.3.4"
RUN apk add --no-cache brotli git libc6-compat \
    && npm install -g pnpm@${PNPM_VERSION}
WORKDIR /umami
RUN git clone --depth 1 --branch "v${UMAMI_VERSION}" https://github.com/umami-software/umami.git . \
    && printf 'strictDepBuilds: false\n' >> pnpm-workspace.yaml \
    && pnpm install --frozen-lockfile
COPY tools/dashboard/prepare.mts /tools/prepare.mts
ARG BASE_PATH
ENV BASE_PATH=$BASE_PATH \
    NEXT_TELEMETRY_DISABLED=1
# Precompressed copies are served to clients that accept them.
RUN pnpm exec tsx /tools/prepare.mts . \
    && pnpm exec next build --turbo \
    && find out -type f -size +1k \( -name '*.html' -o -name '*.js' -o -name '*.css' \
         -o -name '*.json' -o -name '*.txt' -o -name '*.svg' \) \
         -exec gzip -9 -k {} \; -exec brotli -q 11 {} \;

FROM scratch AS dashboard
COPY --from=dashboard-build /umami/out /

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
COPY --from=dashboard / /app/dashboard
# Same layout as upstream's image: a GeoLite2 database mounted at /app/geo is picked up.
WORKDIR /app
# The dashboard only works under the BASE_PATH it was built for.
ARG BASE_PATH
ENV BASE_PATH=$BASE_PATH \
    PORT=3000
EXPOSE 3000
# nobody:nogroup (there is no /etc/passwd to look the name up in).
USER 65534:65534
HEALTHCHECK --interval=5s --timeout=3s --retries=12 \
    CMD ["/usr/local/bin/umami-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/umami-server"]
