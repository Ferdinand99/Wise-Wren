# syntax=docker/dockerfile:1
#
# Wise Wren: two images from one build, both static musl binaries on Alpine,
# rustls only (no OpenSSL in the binaries).
#
#   docker build --target server -t wise-wren:latest .        # the server (also the default)
#   docker build --target agent  -t wise-wren-agent:latest .  # the agent for each Docker host
#
# Needs BuildKit (the default since Docker 23). The cargo registry, the git
# checkout of vole-kit and the target directory live in cache mounts, so
# rebuilds after a source change only recompile Wren itself.

ARG RUST_VERSION=1.99
ARG ALPINE_VERSION=3.22

# --- Builder -----------------------------------------------------------------
FROM rust:${RUST_VERSION}-alpine AS builder

# build-base: C toolchain for the bundled SQLite and aws-lc-rs (rustls crypto).
RUN apk add --no-cache build-base

WORKDIR /app

# Smaller binaries without touching Cargo.toml: no symbols, and link-time
# optimisation across all crates in one unit.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_PROFILE_RELEASE_LTO=true \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    CARGO_TERM_COLOR=never

# Everything the build reads: templates, static files, migrations and locales
# are compiled into the server. vole-kit comes from its git tag (Cargo.lock).
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY templates ./templates
COPY static ./static
COPY migrations ./migrations
COPY locales ./locales
COPY agent ./agent

# BUILD_JOBS (build arg): cap parallel compile jobs on a shared machine.
ARG BUILD_JOBS=
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked ${BUILD_JOBS:+--jobs $BUILD_JOBS} -p wise-wren -p wren-agent \
    && cp target/release/wise-wren target/release/wren-agent /usr/local/bin/

# --- Agent -------------------------------------------------------------------
# Runs on each Docker host, as root, not privileged (docs/spike-findings.md,
# "Decision: agent runtime form"). It carries what later phases run on the
# host's Docker: the Docker CLI with Compose (>= 2.24.4) for Compose and
# script stacks, bash/curl/openssl/coreutils/jq for update scripts such as
# Fluxer's installer and reflink snapshots (GNU cp), and PHP 8.4 for Unraid's
# own update_container (no gettext extension: it clashes with Unraid's _()).
FROM alpine:${ALPINE_VERSION} AS agent

ARG VERSION=0.0.0
ARG REVISION=unknown
ARG CREATED=unknown

LABEL org.opencontainers.image.title="Wise Wren agent" \
      org.opencontainers.image.description="Wise Wren's agent: runs on each Docker host and connects out to Wren" \
      org.opencontainers.image.url="https://github.com/Ferdinand99/Wise-Wren" \
      org.opencontainers.image.source="https://github.com/Ferdinand99/Wise-Wren" \
      org.opencontainers.image.documentation="https://github.com/Ferdinand99/Wise-Wren/blob/main/docs/development.md" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"

RUN apk add --no-cache ca-certificates \
        docker-cli docker-cli-compose \
        bash curl openssl coreutils jq \
        php84 php84-curl php84-dom php84-simplexml php84-xml php84-mbstring \
        php84-posix php84-session php84-ctype \
    && ln -sf /usr/bin/php84 /usr/bin/php

COPY docker/php-wren.ini /etc/php84/conf.d/99-wren.ini
COPY --from=builder /usr/local/bin/wren-agent /usr/local/bin/wren-agent

# agent.toml lives here (/config/agent.toml, the agent's default). It is
# optional: WW_SERVER_URL and WW_AGENT_TOKEN suffice. Don't set
# WW_AGENT_CONFIG here: a path named that way must exist.
WORKDIR /config

ENTRYPOINT ["/usr/local/bin/wren-agent"]

# --- Server ------------------------------------------------------------------
FROM alpine:${ALPINE_VERSION} AS server

ARG VERSION=0.0.0
ARG REVISION=unknown
ARG CREATED=unknown

LABEL org.opencontainers.image.title="Wise Wren" \
      org.opencontainers.image.description="Keeps the Docker containers on your servers up to date, without surprises: one Rust binary, SQLite" \
      org.opencontainers.image.url="https://github.com/Ferdinand99/Wise-Wren" \
      org.opencontainers.image.source="https://github.com/Ferdinand99/Wise-Wren" \
      org.opencontainers.image.documentation="https://github.com/Ferdinand99/Wise-Wren#readme" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"

# ca-certificates: TLS trust store for outgoing HTTPS (notifications).
# tzdata: time zones for display.
# su-exec: the entrypoint drops from root to `wren` after fixing /data ownership.
# Unprivileged user `wren` (uid/gid 1000) owns /data and runs the app.
RUN apk add --no-cache ca-certificates tzdata su-exec \
    && addgroup -S -g 1000 wren \
    && adduser -S -D -H -u 1000 -G wren -h /data -s /sbin/nologin wren \
    && mkdir -p /data \
    && chown wren:wren /data

COPY --from=builder /usr/local/bin/wise-wren /usr/local/bin/wise-wren
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV WW_DATA_DIR=/data \
    WW_PORT=3003

# No `USER` here on purpose: the entrypoint starts as root only to chown /data,
# then runs the app as `wren` via su-exec. Setting `user:` / `--user` skips
# that step entirely.
WORKDIR /data
VOLUME ["/data"]
EXPOSE 3003

# The server answers /healthz (busybox wget, no curl needed).
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -q -O /dev/null "http://127.0.0.1:${WW_PORT:-3003}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
