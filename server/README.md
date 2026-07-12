# license-server

Licensing + Razorpay payment server (`PHASE4_DESIGN.md`). Implements the 7+1
customer-facing endpoints in `API_SPECIFICATION.md` plus the Razorpay
webhook and a background payment-reconciliation job.

## Running with Docker Compose

`docker-compose.yml` (repository root) runs three services on an internal `licensing` network:

- `postgres` — `postgres:16`, persistent named volume (`pgdata`), healthcheck-gated.
- `license-server` — built from `server/Dockerfile` (multi-stage: `rust:1.90-slim-bookworm`
  builder → `debian:bookworm-slim` runtime, non-root user), waits for Postgres to report
  healthy, runs its own migrations at startup (`server/migrations/`), then serves on `:8080`.
  Not published to the host — only reachable through `caddy` or from other containers on the
  `licensing` network.
- `caddy` — `caddy:2-alpine`, terminates TLS and reverse-proxies everything to
  `license-server:8080` (`Caddyfile`, repository root). The only service publishing ports to the
  host (`80`, `443`).

`server/Dockerfile` builds with the **repository root** as its Docker build context (needed to
reach the sibling `protocol/` crate — see `PHASE4_DESIGN.md` §1.1's shared-crate layout) even
though the Dockerfile itself lives in `server/`; `docker-compose.yml` is already wired for this.
It never copies the desktop app's own source (`src/`, `ui/`, `assets/`) into the image.

For **local development without a real domain**, Caddy's automatic HTTPS can't obtain a
certificate (no public DNS to complete the ACME challenge against), so skip it:

```sh
docker compose up -d postgres license-server
```

`license-server` isn't published to the host (only reachable via `caddy` or the internal
`licensing` network — see above), so the simplest local check is `cargo run` against the same
`DATABASE_URL` (see "Running locally without Docker" below), or temporarily add a `ports:
["8080:8080"]` line under `license-server` in a local-only compose override — don't commit that
back.

## Production deployment (VPS)

### DNS requirements

- An A (and/or AAAA) record for your real domain pointing at the VPS's public IP. Replace the
  placeholder `license.example.com` in `Caddyfile` with that domain first — do not commit a real
  domain.
- Ports **80** and **443** reachable from the internet: `80` for Caddy's ACME HTTP-01 challenge
  and the HTTP→HTTPS redirect, `443` for the actual traffic. No other port needs to be open —
  `postgres` and `license-server` are never bound to the host's public interface
  (`docker-compose.yml`'s `licensing` network; `PHASE4_DESIGN.md` §8.1).

### First deployment

1. Provision a VPS with Docker + the Docker Compose plugin installed.
2. Clone this repository onto the VPS.
3. `cp server/.env.example server/.env` and fill in real values — never commit it.
4. Set `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` (shell env, or a repository-root
   `.env` — Docker Compose loads that automatically) matching the credentials embedded in
   `server/.env`'s `DATABASE_URL`.
5. Edit `Caddyfile`: replace `license.example.com` with your real domain.
6. Point that domain's DNS at the VPS (see DNS requirements above) and let it propagate — Caddy
   needs the domain already resolving to this host before its first request, or certificate
   issuance fails.
7. `docker compose up -d --build`.
8. Confirm: `curl -f https://<your-domain>/healthz` and `curl -f https://<your-domain>/readyz`.

### HTTPS certificate generation

Fully automatic — Caddy requests and renews its own certificate (Let's Encrypt by default,
HTTP-01 challenge over `:80`) the first time it sees a request for the domain in `Caddyfile`; no
manual `certbot`/cron step, no config beyond the bare domain name (`Caddyfile`). Certificates and
Caddy's ACME account state live in the `caddy_data` named volume (`caddy_config` holds its
running config), both of which survive `docker compose down` (without `-v`) and container
recreation — so redeploys don't re-trigger issuance or risk hitting Let's Encrypt's rate limits.
Watch first-boot issuance with `docker compose logs -f caddy`.

### Restart procedure

```sh
docker compose restart                 # everything
docker compose restart caddy           # just Caddy, e.g. after editing Caddyfile
docker compose restart license-server  # just the app
```

The server is stateless beyond in-memory rate-limit counters (`PHASE4_DESIGN.md` §8.3) — a
restart never loses committed license/payment data, only in-flight requests and the current
rate-limit window, both acceptable.

### Docker Compose startup

```sh
docker compose up -d --build
```

Starts `postgres` first (waits for its healthcheck), then `license-server` (runs migrations at
startup, before it accepts traffic), then `caddy` (obtains/renews its certificate and starts
proxying). Re-running this after a `git pull` rebuilds only what changed.

### Viewing logs

```sh
docker compose logs -f                 # all services
docker compose logs -f license-server  # app logs (tracing, to stdout/stderr — PHASE4_DESIGN.md §8.3)
docker compose logs -f caddy           # certificate issuance + proxy access logs
docker compose logs -f postgres
```

### Updating the server

```sh
git pull
docker compose up -d --build --no-deps license-server
```

`--no-deps` rebuilds/restarts only `license-server`, leaving `postgres` and `caddy` untouched —
matches `PHASE4_DESIGN.md` §8.3's accepted trade-off of a few seconds of downtime during the
restart, no blue/green machinery this phase. `server/deploy/deploy.sh` wraps this with a
`server/.env` sanity check and a post-deploy log tail.

### Backup location

Nightly `pg_dump`, written by `server/deploy/backup.sh` to `/var/backups/license-server` on the
VPS (outside every Docker volume, so a `docker compose down -v` or `pgdata` volume loss can't
take backups with it) — split into `daily/` (14 kept) and `weekly/` (8 kept, Sunday's daily dump)
subdirectories, per `PHASE4_DESIGN.md` §8.2/§14 item 10. That directory should also be synced
off-VPS (object storage, another host, etc.); `backup.sh` has a marked `TODO` hook for this since
the design doc doesn't fix a specific destination. `server/deploy/restore.sh` restores from a
dump it produced.

## Configuration

All configuration is environment variables, read once at startup
(`server/src/config.rs`) — see `server/.env.example` for the full list and defaults. Only
`DATABASE_URL` is required; the Razorpay variables are optional (Razorpay integration is
simply unavailable, not a startup failure, when unset).

## Running locally without Docker

```sh
cd server
cargo run
```

Requires `DATABASE_URL` pointing at a real Postgres instance in the environment (or in an
uncommitted `server/.env` loaded by your own shell/tooling — this crate does not load `.env`
files itself).

## Tests

```sh
cargo fmt -p license-server -p license-protocol
cargo clippy -p license-server --all-targets -- -D warnings
cargo test -p license-server
```

Some integration tests require a real Postgres and are marked `#[ignore]` by default
(`PHASE4_DESIGN.md` §9) — run them explicitly with `cargo test -p license-server -- --ignored`
against a real database.
