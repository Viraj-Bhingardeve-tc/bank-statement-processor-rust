# license-server

Licensing + Razorpay payment server (`PHASE4_DESIGN.md`). Implements the 7+1
customer-facing endpoints in `API_SPECIFICATION.md` plus the Razorpay
webhook and a background payment-reconciliation job.

## Running with Docker Compose

From the repository root:

```sh
cp server/.env.example server/.env
# edit server/.env with real values — never commit it

export POSTGRES_USER=license_server
export POSTGRES_PASSWORD=changeme   # or put these in a repo-root .env instead
export POSTGRES_DB=license_server

docker compose up -d --build
```

This starts two services (`docker-compose.yml`, repository root):

- `postgres` — `postgres:16`, persistent named volume (`pgdata`), healthcheck-gated.
- `license-server` — built from `server/Dockerfile` (multi-stage: `rust:1.90-slim-bookworm`
  builder → `debian:bookworm-slim` runtime, non-root user), waits for Postgres to report
  healthy, runs its own migrations at startup (`server/migrations/`), then serves on `:8080`.

`server/Dockerfile` builds with the **repository root** as its Docker build context (needed to
reach the sibling `protocol/` crate — see `PHASE4_DESIGN.md` §1.1's shared-crate layout) even
though the Dockerfile itself lives in `server/`; `docker-compose.yml` is already wired for this.
It never copies the desktop app's own source (`src/`, `ui/`, `assets/`) into the image.

Check it came up:

```sh
curl http://localhost:8080/healthz
curl http://localhost:8080/readyz   # verifies DB connectivity too
```

Stop everything (keeps the `pgdata` volume, so license/payment data survives):

```sh
docker compose down
```

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
