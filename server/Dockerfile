# syntax=docker/dockerfile:1
#
# Production image for the license-server binary (PHASE4_DESIGN.md §8).
# Build from the REPOSITORY ROOT so both workspace members are reachable:
#
#   docker build -f server/Dockerfile -t license-server .
#
# docker-compose.yml (repository root) is wired to do exactly this.

########################################################################
# Builder — full Rust toolchain, discarded after the binary is built.
########################################################################
FROM rust:1.90-slim-bookworm AS builder

WORKDIR /build

# Workspace manifest + lockfile first (PHASE4_DESIGN.md §1.1: root
# Cargo.toml carries the `[workspace]` table listing `protocol` and
# `server`) — copied ahead of the crate sources so a source-only edit
# doesn't invalidate Docker's dependency-fetch layer.
COPY Cargo.toml Cargo.lock ./

# Only the two crates license-server actually depends on. The desktop
# app's own source (src/, ui/, assets/, tests/) is never copied into this
# image — its Cargo.toml `[package]`/`[[bin]]`/`[lib]` targets don't need
# to exist on disk for `cargo build -p license-server` to resolve the
# workspace and build just this package (verified: an unselected
# workspace member's target paths are never checked unless that member is
# actually compiled).
COPY protocol ./protocol
COPY server ./server

# reqwest's default native-tls backend pulls in openssl-sys, which needs
# pkg-config + libssl-dev to *compile* — the runtime stage's libssl3
# further below is only for linking the already-built binary, not for
# building it, so this builder stage needs its own copy of both.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config \
       libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# --locked: fail the build rather than silently re-resolving dependency
# versions that drift from the committed Cargo.lock — this image should
# build from exactly what was tested locally, every time.
RUN cargo build --release --locked -p license-server

########################################################################
# Runtime — small Debian base, compiled binary only.
########################################################################
FROM debian:bookworm-slim AS runtime

# ca-certificates: validates outbound TLS to Postgres/Razorpay.
# libssl3: reqwest's default TLS backend (native-tls) links against the
# system OpenSSL on Linux; sqlx's own Postgres connection uses rustls
# (server/Cargo.toml) and needs neither, but reqwest does.
# busybox-static: Production Hardening, Finding H6 — this runtime image
# previously had no HTTP client at all (no curl/wget/busybox), so the
# HEALTHCHECK below (which needs one to probe `/healthz`) required adding
# exactly one. `busybox-static` is a single, fully statically-linked
# binary with no further shared-library dependencies of its own — unlike
# `curl` (pulls in libcurl4 plus a longer transitive chain: libnghttp2,
# libssh2-1, libldap, ...) or GNU `wget` (a smaller but still nonzero chain
# via libidn2/libpsl), it adds nothing beyond this one package. Its `wget`
# applet still correctly fails (non-zero exit) on both a connection
# failure/timeout and a non-2xx HTTP status — exactly what the check
# below needs, at the smallest footprint of the options considered.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 busybox-static curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 --shell /usr/sbin/nologin license-server

COPY --from=builder /build/target/release/license-server /usr/local/bin/license-server

USER license-server

# All configuration is read from the process environment at startup
# (server/src/config.rs): DATABASE_URL (required), HOST, PORT, RUST_LOG,
# DATABASE_MAX_CONNECTIONS, and the optional RAZORPAY_* / plan-id vars —
# see server/.env.example. None are baked into this image.

EXPOSE 8080

# Production Hardening, Finding H6: without this, a hung-but-listening
# process (deadlock, poisoned internal state, stuck migration) was never
# detected or auto-restarted — Docker/Compose only ever knew the container
# hadn't exited, never whether it was actually answering HTTP.
# `/healthz` (not `/readyz`): a plain liveness probe with no database
# dependency (`routes::health`'s own doc comment) — appropriate here since
# `HOST` defaults to `0.0.0.0` (server/.env.example), so `127.0.0.1:8080`
# is always reachable from inside this same container regardless of what
# interface the process is actually listening on.
#   --interval=30s:      matches docker-compose.yml's Postgres healthcheck
#                         cadence — an app-tier check doesn't need to run
#                         more often than that.
#   --timeout=5s:         generous for a same-container loopback request;
#                         the busybox wget call itself times out sooner
#                         (see -T 3 below), so a hung connection is always
#                         reported as a clean CMD failure, never something
#                         Docker has to kill mid-probe.
#   --start-period=10s:   matches Postgres's own start_period — gives the
#                         process time to run migrations (main.rs) before
#                         an early failure could count toward --retries.
#   --retries=3:          three consecutive failures (~90s of sustained
#                         unreachability at this interval) before Docker
#                         marks the container unhealthy — short enough to
#                         act on, long enough not to flap on one slow tick.
#   wget -q:              suppresses busybox wget's own progress output;
#                         Docker already discards a successful CMD's
#                         stdout/stderr, so this only prevents noise
#                         appearing in `docker inspect`'s captured log on
#                         every run.
#   wget -T 3:            a 3s request timeout, comfortably under this
#                         instruction's own --timeout=5s.
#   -O /dev/null:          discards the response body — this check only
#                         cares about the HTTP outcome, not the content.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS --max-time 3 http://127.0.0.1:8080/healthz >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/license-server"]
