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
3. `cp server/.env.example server/.env` and fill in real values — never commit it. For the first
   deployment, point `DATABASE_URL` at the **admin** account (`POSTGRES_USER`/`POSTGRES_PASSWORD`
   below) temporarily — migrations (including the one that creates the restricted role) need to
   run as admin at least once; see "Database roles and least privilege" below.
4. Set `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` (shell env, or a repository-root
   `.env` — Docker Compose loads that automatically) matching the credentials embedded in
   `server/.env`'s `DATABASE_URL` from the previous step.
5. Edit `Caddyfile`: replace `license.example.com` with your real domain.
6. Point that domain's DNS at the VPS (see DNS requirements above) and let it propagate — Caddy
   needs the domain already resolving to this host before its first request, or certificate
   issuance fails.
7. `docker compose up -d --build`.
8. Confirm: `curl -f https://<your-domain>/healthz` and `curl -f https://<your-domain>/readyz`.
9. Set the restricted role's password (`server/deploy/set-app-db-password.sh`, using
   `APP_DB_USER`/`APP_DB_PASSWORD`), then switch `server/.env`'s `DATABASE_URL` to that role and
   restart `license-server` — see "Database roles and least privilege" below for exactly why this
   is safe to do right away, including for the automatic migration step at every future startup.

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

**Restore-safety and corruption protection (Phase 4J.2):** `pg_dump` runs with `--clean
--if-exists`, so a dump can be restored straight into an already-populated database — `DROP ...
IF EXISTS` before each `CREATE` — instead of erroring on "already exists" or silently appending
duplicate rows via `COPY` into tables nothing dropped first. `backup.sh` writes to a `.partial`
temp file, verifies it with `gzip -t`, and only then renames it into `daily/`/`weekly/` — a failed
or truncated dump never leaves a corrupt or half-written file at a path `restore.sh` or the
retention pruning would treat as a real backup (any `.partial` left by a hard kill mid-run is
swept up at the start of the next scheduled run). `restore.sh` mirrors this on the way in: it
runs `gzip -t` on the given file before touching Docker at all, and restores via
`psql -v ON_ERROR_STOP=1`, so a corrupt input file or any SQL error during the restore itself
aborts immediately instead of silently leaving a partially-restored database. Run
`bash server/deploy/test-backup-restore.sh` to exercise both scripts offline against a fake
`docker` shim (no VPS, containers, or real Postgres needed) — it asserts the flags above are
actually present and that a simulated dump/restore failure leaves nothing behind.

## Database roles and least privilege

**Phase 4J.8 (production readiness audit):** by default, `POSTGRES_USER` is the Postgres
*instance superuser* the official `postgres` Docker image bootstraps on first init — full
`SUPERUSER`/`CREATEDB`/`CREATEROLE`/`REPLICATION`/`BYPASSRLS` reach, able to run `ALTER SYSTEM`,
read or modify any database on the instance. Before this phase, `license-server`'s own
`DATABASE_URL` connected using that exact account — a compromised server process had the same
reach as a database administrator. It now connects as a separate, narrowly-scoped role instead;
`postgres`/`POSTGRES_USER` remains for administration only (running migrations, `pg_dump`/`psql`
in `server/deploy/backup.sh`/`restore.sh`, and creating the restricted role in the first place).

### How the role is created

`server/migrations/0003_least_privilege_app_role.sql` creates `license_server_app` (idempotent —
safe to run against a fresh database or one that already has it) and grants it exactly:

- `CONNECT` on the database
- `USAGE` **and `CREATE`** on schema `public` (see "Why `CREATE` on the schema is granted" below —
  this is the one deliberate exception to an otherwise DML-only role, and it is *not* a general
  license to create arbitrary objects; it exists for one specific, documented reason)
- `SELECT`, `INSERT`, `UPDATE`, `DELETE` on every application table (`users`, `subscriptions`,
  `licenses`, `devices`, `sessions`, `payments`, `payment_webhook_events`) — enumerated
  explicitly, not `ALL TABLES IN SCHEMA public`, so the grant can never silently widen to some
  future unrelated table
- `USAGE`/`SELECT` on each table's `BIGSERIAL` sequence (needed for `INSERT` to work at all)
- `SELECT`/`INSERT`/`UPDATE` on sqlx's own `_sqlx_migrations` bookkeeping table
- `ALTER DEFAULT PRIVILEGES` extending the same four table/sequence privileges to anything a
  *future* migration creates, so later purely-additive migrations don't need their own follow-up
  grant

Deliberately **never** granted: `SUPERUSER`, `CREATEDB`, `CREATEROLE`, `REPLICATION`,
`BYPASSRLS` (explicitly written into the `CREATE ROLE` statement as `NO...`, not just relying on
defaults), or anything that would allow `ALTER SYSTEM` (which itself requires `SUPERUSER`, so
withholding that already withholds it). This migration runs automatically like every other one in
this directory (`server/src/db.rs`'s `sqlx::migrate!()`, applied at startup by whichever
connection `DATABASE_URL` currently points at — which must be the admin account the *first* time,
since creating a role and granting privileges on tables it doesn't own both require elevated
privilege the restricted role itself will never have).

### Why `CREATE` on the schema is granted

`server/src/main.rs` calls `db::run_migrations(&pool)` unconditionally on every process start,
using whatever `DATABASE_URL` is configured. sqlx's migrator unconditionally issues `CREATE TABLE
IF NOT EXISTS _sqlx_migrations (...)` as its first step on **every single run** — Postgres
requires `CREATE` privilege on the schema to even attempt that statement, regardless of whether
the table already exists or any new migration is actually pending. Without this grant,
`license_server_app` could not be used as `DATABASE_URL` at all: the server would fail at startup
with "permission denied for schema public" before ever reaching a real query, on every restart,
forever.

This is granted specifically and only for that reason — `CREATE ON SCHEMA public` lets this role
create objects inside this one schema, in this one database, and nothing more. It does not grant
`CREATEDB` (create other *databases*), `CREATEROLE` (create other *roles*), `REPLICATION`,
`BYPASSRLS`, or `SUPERUSER` (and therefore not `ALTER SYSTEM` either, which requires it) — every
item on the "do not grant" list stays withheld. A compromised `license-server` process can create
or alter objects in its own schema, which is a real (if narrow) increase in blast radius over pure
DML — but it still cannot touch another database, mint itself a more powerful role, replicate the
cluster, bypass row-level security, or rewrite server-wide configuration.

### How passwords are configured

The migration above deliberately never sets a password — a `.sql` file lives in version control,
and a real secret has no business being in it. Set (and later rotate) it with:

```sh
APP_DB_USER=license_server_app APP_DB_PASSWORD='a-real-secret' server/deploy/set-app-db-password.sh
```

(or export `APP_DB_USER`/`APP_DB_PASSWORD`, or put them in `server/.env`, which the script also
sources, before running it with no arguments). This connects as the admin account
(`POSTGRES_USER`) and runs `ALTER ROLE license_server_app WITH LOGIN PASSWORD ...` — safe to
re-run any time, since it simply overwrites the previous password rather than accumulating state.
Until this has been run at least once, `license_server_app` exists but cannot log in at all.

Once the migration has applied (creating the role) and this script has set its password,
`server/.env`'s `DATABASE_URL` should be switched to `license_server_app` — **not**
`POSTGRES_USER` — for normal operation, including the automatic migration step at every future
startup (see the previous section: the schema-`CREATE` grant means this now works without an
admin connection). The recommended sequence, in order:

1. Bring up `postgres` (and, for the first deploy, `license-server` pointed at the admin account
   just long enough to apply migrations — see "First deployment" above).
2. Confirm migration `0003_least_privilege_app_role.sql` applied (`docker compose logs
   license-server` shows "database migrations applied").
3. `server/deploy/set-app-db-password.sh` to set `license_server_app`'s password.
4. Update `server/.env`'s `DATABASE_URL` to `postgres://license_server_app:<password>@postgres:5432/<db>`.
5. `docker compose restart license-server`.

### Why least privilege is used

If the `license-server` process itself is ever compromised (a dependency vulnerability, a bug in
request handling, a leaked container), the blast radius should be limited to reading/writing this
server's own 7 tables (plus creating objects in its own schema, per the one documented exception
above) — not creating or dropping arbitrary databases, creating new roles, initiating
replication, bypassing row-level security, or rewriting the Postgres instance's own configuration
via `ALTER SYSTEM`. `PHASE4_DESIGN.md` §5 states this requirement directly: "the server's Postgres
role should have INSERT/SELECT/UPDATE on its own tables and nothing else... a compromised server
process shouldn't be able to do more damage than the application logic itself already could."

### How to rotate the application password

Run `server/deploy/set-app-db-password.sh` again with a new `APP_DB_PASSWORD`, update
`server/.env`'s `DATABASE_URL` to match, then `docker compose restart license-server` to pick up
the new value (`env_file` is only read at container start, not live-reloaded). The old password
stops working the instant `ALTER ROLE` completes, so restart promptly to avoid a connection gap.

## Configuration

All configuration is environment variables, read once at startup
(`server/src/config.rs`) — see `server/.env.example` for the full list and defaults. Only
`DATABASE_URL` is required; the Razorpay variables are optional (Razorpay integration is
simply unavailable, not a startup failure, when unset).

## Monitoring

Three unauthenticated operational endpoints, in addition to the customer-facing API
(`API_SPECIFICATION.md`) — none of them require a bearer token, matching each other's existing
precedent (`PHASE4_DESIGN.md` §8.3):

| Endpoint     | Purpose                                                                                   |
|--------------|--------------------------------------------------------------------------------------------|
| `GET /healthz` | Liveness — "the process is running." No database dependency. Used by Docker's `healthcheck:` directive. |
| `GET /readyz`  | Readiness — liveness **and** the database is reachable. `503` (not `500`) when the database can't be queried. |
| `GET /metrics` | Prometheus-compatible scrape endpoint (Phase 4I.2), described below.                        |

### `GET /metrics`

Returns the current process's metrics in Prometheus text exposition format
(`Content-Type: text/plain; version=0.0.4; charset=utf-8`). Point a Prometheus server at it with a
scrape config along these lines:

```yaml
scrape_configs:
  - job_name: license-server
    metrics_path: /metrics
    static_configs:
      - targets: ["license.example.com:443"]
    scheme: https
```

Metrics exposed (every counter/gauge/histogram also carries a `# HELP`/`# TYPE` line in the
scrape itself — `server/src/observability.rs` is the source of truth if this table and the code
ever drift):

| Metric | Type | Labels | What it means |
|---|---|---|---|
| `http_requests_total` | counter | `method`, `path`, `status` | Every HTTP request handled, by matched route, not raw URI (bounded label cardinality — an unmatched/404 request falls back to the raw path). |
| `http_request_duration_seconds` | histogram | `method`, `path`, `status` | Request latency. |
| `http_requests_in_flight` | gauge | — | Requests currently being handled (a sudden climb with no matching drop is a stuck-handler/slow-downstream signal — e.g. Razorpay hanging on `/create-checkout-session`). |
| `webhook_requests_total` | counter | `outcome` | Every inbound call to `/webhooks/razorpay`, by outcome (`processed`, `missing_signature`, `invalid_signature`, `not_configured`, `invalid_payload`, `processing_error`). A nonzero rate of anything but `processed` is worth alerting on — see §4/§5's webhook-trust-boundary reasoning in `PHASE4_DESIGN.md`. |
| `webhook_events_total` | counter | `event_type` | Successfully processed Razorpay events, by type (`payment.captured`, `subscription.charged`, etc.). |
| `reconciliation_runs_total` | counter | `result` (`success`/`failure`) | Every reconciliation job tick (`PHASE4_DESIGN.md` §12) — alert if there's been no `success` in longer than a few tick intervals (15 minutes each). |
| `reconciliation_payments_checked_total` | counter | — | Cumulative Razorpay payments inspected across all runs. |
| `reconciliation_payments_healed_total` | counter | — | Cumulative payments healed — i.e. a webhook never arrived and this job caught it instead. A sustained nonzero rate here means webhook delivery is unhealthy even though the system is still self-correcting. |
| `db_pool_connections` | gauge | — | Current total connections (idle + in-use) held by the Postgres pool. Computed at scrape time from `PgPool::size()` — not a background poller. |
| `db_pool_idle_connections` | gauge | — | Current idle connections. `db_pool_connections - db_pool_idle_connections` is connections actually in use. |

`/metrics` never exposes secrets or per-customer data — every value is an aggregate
count/duration/gauge (see `server/src/observability.rs`'s own doc comment). It goes through the
same HTTP-metrics middleware as every other route, so scraping it also counts as one more
`http_requests_total{path="/metrics", ...}` observation.

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

`tests/least_privilege_role.rs` (Phase 4J.8) specifically verifies the least-privilege role
migration against a *real* Postgres, connected as an admin account — it asserts
`license_server_app` can read/write its own tables, cannot create a table, and has none of
`SUPERUSER`/`CREATEDB`/`CREATEROLE`/`REPLICATION`/`BYPASSRLS` set. This can't be meaningfully
faked with a mock connection (permission enforcement happens inside Postgres itself), so — per
this phase's own instructions — there is deliberately no non-`#[ignore]`d automated coverage for
it; that is a documented limitation of this test suite, not an oversight.
