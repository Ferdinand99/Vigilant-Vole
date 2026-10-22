# syntax=docker/dockerfile:1
#
# Vigilant Vole — single static musl binary on Alpine, rustls only (no OpenSSL).
#
#   docker build -t vigilant-vole:latest .
#
# Needs BuildKit (the default since Docker 23). The cargo registry and the
# target directory live in cache mounts, so rebuilds after a source change only
# recompile the crate itself.

ARG RUST_VERSION=1.99
ARG ALPINE_VERSION=3.22

# --- Builder -----------------------------------------------------------------
FROM rust:${RUST_VERSION}-alpine AS builder

# build-base: C toolchain for the bundled SQLite and aws-lc-rs (rustls crypto).
RUN apk add --no-cache build-base

WORKDIR /app

# Smaller binary without touching Cargo.toml.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_TERM_COLOR=never

# Everything the build reads: templates, static assets, migrations and
# locales are compiled into the binary. examples/ only so Cargo can load the
# workspace (kit-demo is a member); it isn't built.
COPY Cargo.toml Cargo.lock build.rs ./
COPY kit ./kit
COPY examples ./examples
COPY src ./src
COPY templates ./templates
COPY static ./static
COPY migrations ./migrations
COPY locales ./locales

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked -p vigilant-vole \
    && cp target/release/vigilant-vole /usr/local/bin/vigilant-vole

# --- Runtime -----------------------------------------------------------------
FROM alpine:${ALPINE_VERSION} AS runtime

ARG VERSION=0.1.0
ARG REVISION=unknown
ARG CREATED=unknown

LABEL org.opencontainers.image.title="Vigilant Vole" \
      org.opencontainers.image.description="Lightweight self-hosted uptime monitor: one Rust binary, one SQLite file" \
      org.opencontainers.image.url="https://github.com/Ferdinand99/Vigilant-Vole" \
      org.opencontainers.image.source="https://github.com/Ferdinand99/Vigilant-Vole" \
      org.opencontainers.image.documentation="https://github.com/Ferdinand99/Vigilant-Vole#readme" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"

# ca-certificates: TLS trust store for HTTPS checks and notifications.
# tzdata: maintenance windows and display time zones.
# su-exec: the entrypoint drops from root to `vole` after fixing /data ownership.
# Unprivileged user `vole` (uid/gid 1000) owns /data and runs the app.
RUN apk add --no-cache ca-certificates tzdata su-exec \
    && addgroup -S -g 1000 vole \
    && adduser -S -D -H -u 1000 -G vole -h /data -s /sbin/nologin vole \
    && mkdir -p /data \
    && chown vole:vole /data

COPY --from=builder /usr/local/bin/vigilant-vole /usr/local/bin/vigilant-vole
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV DATA_DIR=/data \
    PORT=3001

# No `USER` here on purpose: the entrypoint starts as root only to chown /data
# (so data from older root-run images stays writable), then runs the app as
# `vole` via su-exec. Setting `user:` / `--user` skips that step entirely.
WORKDIR /data
VOLUME ["/data"]
EXPOSE 3001

# Ping monitors use unprivileged ICMP sockets (Docker allows them for every
# user by default via net.ipv4.ping_group_range) and fall back to raw sockets.
#
# Health: the server answers /healthz (busybox wget, no curl needed). The same
# image also runs remote agents (`agent ...`), which have no HTTP listener:
# they count as healthy while the process is up.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD if tr '\0' '\n' < /proc/1/cmdline | grep -qx agent; then exit 0; fi; \
        wget -q -O /dev/null "http://127.0.0.1:${PORT:-3001}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
