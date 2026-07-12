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
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 --shell /usr/sbin/nologin license-server

COPY --from=builder /build/target/release/license-server /usr/local/bin/license-server

USER license-server

# All configuration is read from the process environment at startup
# (server/src/config.rs): DATABASE_URL (required), HOST, PORT, RUST_LOG,
# DATABASE_MAX_CONNECTIONS, and the optional RAZORPAY_* / plan-id vars —
# see server/.env.example. None are baked into this image.

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/license-server"]
