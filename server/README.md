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

### Razorpay production setup

Steps 3/9 of "First deployment" above get `server/.env`'s Razorpay variables *present*; this is
what makes them *correct* for a live, paying-customer deployment. Do this after `license-server`
is reachable over HTTPS (step 8) but before advertising checkout to real customers.

1. **Switch to live API keys.** In the Razorpay dashboard, generate a live key pair (Settings →
   API Keys → Generate Live Key) — `rzp_live_...` — and set `RAZORPAY_KEY_ID`/`RAZORPAY_KEY_SECRET`
   in `server/.env` to it. Test-mode keys (`rzp_test_...`) never charge a real card; a production
   deployment left on one silently takes no real money while still returning what looks like a
   successful checkout URL to customers.
2. **Create the two recurring Plans and set their ids.** `monthly`/`yearly` checkout goes through
   Razorpay's Subscriptions API under a live key (`server/src/razorpay/client.rs`'s
   `HttpRazorpayClient::create_checkout` — the Payment-Links fallback that lets local development
   skip this step **only applies under a test-mode key**), which needs a real, dashboard-created
   Plan, not just an amount. In the Razorpay dashboard: Subscriptions → Plans → Create Plan, once
   for the monthly price and once for the yearly price (amounts must match
   `server/src/service/payment_service.rs`'s `PRICING` table, or the price a customer is charged
   at Razorpay's checkout won't match what this server records/entitles). Copy each Plan's `plan_...`
   id into `RAZORPAY_MONTHLY_PLAN_ID`/`RAZORPAY_YEARLY_PLAN_ID`. **Required, not optional, once
   `RAZORPAY_KEY_ID` is a live key** — `AppConfig::from_vars` refuses to start
   (`ConfigError::LiveRazorpayKeyMissingPlanIds`) rather than let this surface later as a failed
   checkout for whichever plan a real customer happens to pick first.
3. **Register the webhook and set its secret.** In the Razorpay dashboard: Settings → Webhooks →
   Add New Webhook.
   - URL: `https://<your-domain>/webhooks/razorpay`.
   - Secret: generate a strong random value yourself (this is *not* `RAZORPAY_KEY_SECRET` — it's
     a separate value both you and Razorpay hold, used only to HMAC-sign webhook bodies); set the
     same value as `RAZORPAY_WEBHOOK_SECRET` in `server/.env`. Until this is set,
     `POST /webhooks/razorpay` rejects every call outright (`server/src/routes/payment.rs`) rather
     than accepting an unverifiable one.
   - Active events — enable at least every event `service::payment_service::process_webhook_event`
     branches on, since anything else is silently ignored (received, acknowledged, no state
     change): `payment.captured`, `payment.failed`, `subscription.activated`,
     `subscription.charged`, `subscription.cancelled`, `subscription.halted`, `refund.created`,
     `refund.processed`, `payment.dispute.created`, `payment.dispute.closed`.
4. **Restart and verify.** `docker compose up -d --build --no-deps license-server` to pick up the
   new `.env` values, then run a real end-to-end purchase (small/refundable, or Razorpay's
   documented live-mode test path) for each of `monthly`/`yearly`/`lifetime` and confirm: the
   checkout URL opens, the webhook arrives (`docker compose logs -f license-server` should show
   "razorpay webhook processed"), and the license activates. This is the one thing automated tests
   in this repository cannot substitute for — they run against a mocked `RazorpayClient`, never a
   real Razorpay account (see `server/src/razorpay/client.rs`'s module doc comment).
5. Reconciliation (`server/src/reconciliation.rs`) and the operator checklist below still apply
   unchanged in live mode — nothing about switching to live keys/Plans changes their configuration.

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

Nightly `pg_dump`, written by `server/deploy/backup.sh` to `/var/backups/license-server` (override
with `BACKUP_DIR`) on the VPS (outside every Docker volume, so a `docker compose down -v` or
`pgdata` volume loss can't take backups with it) — split into `daily/` (`DAILY_RETENTION`, default
14) and `weekly/` (`WEEKLY_RETENTION`, default 8, Sunday's daily dump) subdirectories, per
`PHASE4_DESIGN.md` §8.2/§14 item 10. `server/deploy/restore.sh` restores from a dump it produced;
`server/deploy/verify-backup.sh` checks one without restoring anything.

**Restore-safety and corruption protection (Phase 4J.2):** `pg_dump` runs with `--clean
--if-exists`, so a dump can be restored straight into an already-populated database — `DROP ...
IF EXISTS` before each `CREATE` — instead of erroring on "already exists" or silently appending
duplicate rows via `COPY` into tables nothing dropped first. `backup.sh` writes to a `.partial`
temp file, verifies it with `gzip -t`, and only then renames it into `daily/`/`weekly/` — a failed
or truncated dump never leaves a corrupt or half-written file at a path `restore.sh` or the
retention pruning would treat as a real backup (any leftover temp file from a hard kill mid-run is
swept up at the start of the next scheduled run).

**Encryption, integrity, and off-site replication (Phase 4L.2.1 — closes the last Critical finding
in `FINAL_PRODUCTION_VALIDATION_REPORT.md`):**

- **Integrity, unconditionally:** every dump gets a `<dump>.sha256` and `<dump>.meta.json` sidecar
  (database name, Postgres server version, size, SHA-256, an `encrypted` flag, and a
  `backup_version` for future format changes). This is independent of `gzip -t`'s own stream
  check — it detects bit-rot or tampering *after* the file was written (e.g. during off-site
  transfer or while sitting on disk), which a bare `gzip -t` cannot. Fully backward compatible: a
  backup made before this phase simply has no sidecars, and `restore.sh`/`verify-backup.sh` treat
  that as a warning, not a failure.
- **Encryption at rest, opt-in:** set `BACKUP_ENCRYPTION_KEY` (a passphrase) and every subsequent
  backup is AES-256-CBC-encrypted (`openssl enc -pbkdf2`), producing `<dump>.sql.gz.enc` instead of
  `<dump>.sql.gz`. Unset (the default) is byte-for-byte the same unencrypted output as before.
  `restore.sh`/`verify-backup.sh` auto-detect encryption from the metadata sidecar (or the `.enc`
  extension, if the sidecar is ever lost) and decrypt transparently — the same
  `BACKUP_ENCRYPTION_KEY` must be set in the environment they run in.
- **Off-site replication, opt-in:** set `OFFSITE_SYNC_CMD` to any shell command your operations
  already use — it's run after a successful local backup, with `BACKUP_DIR` exported for it to
  reference. This repo doesn't hardcode or depend on a specific tool; pick whichever you already
  operate (examples below). Unset (the default) means backups still exist only on this host, now
  with an explicit reminder printed instead of a silent gap:
  ```sh
  # rclone to any rclone-supported remote
  OFFSITE_SYNC_CMD='rclone sync "$BACKUP_DIR" remote:license-server-backups'
  # AWS S3 (or an S3-compatible provider)
  OFFSITE_SYNC_CMD='aws s3 sync "$BACKUP_DIR" s3://your-bucket/license-server-backups'
  # a second host over SSH
  OFFSITE_SYNC_CMD='rsync -az "$BACKUP_DIR"/ backup-host:/backups/license-server/'
  ```
  A failed off-site sync makes `backup.sh` itself exit non-zero (so cron/monitoring notices) even
  though the local backup already succeeded and is safe — the failure message distinguishes the
  two so this isn't mistaken for a failed backup.
- **Safer restores:** `restore.sh` verifies the backup's checksum (if present), decrypts it (if
  encrypted), and runs `gzip -t` — in that order, before touching Docker at all — then restores via
  `psql -v ON_ERROR_STOP=1 --single-transaction`. The single-transaction wrap means a failure
  partway through a restore now rolls back cleanly instead of leaving a half-dropped,
  half-restored database (`pg_dump --clean --if-exists`'s DDL is fully transactional in Postgres,
  so this changes nothing about a successful restore).
- **Dry-run / standalone verification:** `restore.sh --verify-only <file>` runs every check above
  and exits without prompting, stopping the server, or touching the database — or use
  `server/deploy/verify-backup.sh <file>` directly, which needs no Docker/Postgres at all. Good for
  a scheduled DR drill or for checking an off-site copy before trusting it.

Run `bash server/deploy/test-backup-restore.sh` to exercise all of the above offline against a
fake `docker` shim (no VPS, containers, or real Postgres needed) — it asserts the flags/behavior
described above and that a simulated dump/restore/checksum/encryption failure leaves nothing
behind or is rejected before anything destructive happens.

### Operator checklist

- [ ] `server/deploy/backup.sh` runs nightly via cron (`REPO_DIR=/opt/license-server
      /opt/license-server/server/deploy/backup.sh`) and its log/exit code is monitored.
- [ ] `BACKUP_ENCRYPTION_KEY` is set to a real secret (stored outside this repo, e.g. in the host's
      cron environment or a secrets manager) and backed up itself — a lost key makes every
      encrypted backup permanently unreadable.
- [ ] `OFFSITE_SYNC_CMD` is configured and its target verified reachable from the VPS.
- [ ] `bash server/deploy/test-backup-restore.sh` passes (CI or a pre-deploy check).
- [ ] `server/deploy/verify-backup.sh` (or `restore.sh --verify-only`) has been run at least once
      against a real, current production backup — not just the offline test fixtures.
- [ ] A full restore drill (`restore.sh` against a real backup, into a disposable/staging Postgres)
      has been performed at least once and its result documented.
- [ ] `DAILY_RETENTION`/`WEEKLY_RETENTION` match your actual recovery-point-objective needs.

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

### Deploying a schema-altering migration (Phase 4L.3)

`license_server_app` deliberately has no `ALTER`/ownership on the tables it uses (see "Why least
privilege is used" above) — only DML plus the one documented `CREATE ON SCHEMA public` exception.
A future migration that runs `ALTER TABLE` (adding/dropping a column or constraint — e.g.
`migrations/0004_add_payment_dispute_support.sql` was the first to do this) **will fail** with a
Postgres "permission denied" (SQLSTATE `42501`) error if `DATABASE_URL` is already pointed at
`license_server_app` when it runs — `db::run_migrations` (called unconditionally at every startup)
has no way around this, by design; granting the role table ownership would undo the whole point of
this section. `license-server` detects this specific case at startup and logs an actionable hint
alongside the raw Postgres error, but the safe deploy sequence for any release containing a new
`ALTER TABLE`-shaped migration is:

1. Temporarily point `server/.env`'s `DATABASE_URL` back at the admin/superuser account (same
   value used in "First deployment" step 3 above).
2. Deploy/restart `license-server` once — this applies the pending migration(s) while still
   connected as an account that owns the tables.
3. Confirm the logs show "database migrations applied" with no error.
4. Switch `DATABASE_URL` back to `license_server_app` and restart again.

A purely additive migration (new table, new column-less-DEFAULT-only-via-a-separate-`UPDATE`
pattern, etc.) does not need this — only `ALTER TABLE`/`DROP`/constraint changes on existing,
already-owned-by-the-admin-account tables do.

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
against a real database. `cargo test --workspace` (the whole-repo command every developer already
runs) never touches these — they only run when `--ignored` is passed explicitly.

`tests/least_privilege_role.rs` (Phase 4J.8) specifically verifies the least-privilege role
migration against a *real* Postgres, connected as an admin account — it asserts
`license_server_app` can read/write its own tables, cannot create a table, and has none of
`SUPERUSER`/`CREATEDB`/`CREATEROLE`/`REPLICATION`/`BYPASSRLS` set. This can't be meaningfully
faked with a mock connection (permission enforcement happens inside Postgres itself), so — per
this phase's own instructions — there is deliberately no non-`#[ignore]`d automated coverage for
it; that is a documented limitation of this test suite, not an oversight.

### CI: how the Postgres-backed suite runs automatically (Production Hardening, Finding H7)

Before this finding was closed, every test above requiring a real Postgres was `#[ignore]`d and
**never ran in CI at all** — only a developer who happened to run `-- --ignored` against their own
local database ever exercised transactions, migrations, the least-privilege role, or the webhook/
reconciliation flows. `.github/workflows/ci.yml`'s `db-tests` job now runs this suite on every push
to `main` and every pull request:

1. Starts `postgres:16` as a GitHub Actions **service container** (not Docker Compose — this
   repo's `docker-compose.yml` is a production-deployment topology where Postgres is never
   published to the host, not a CI fixture) and waits for its own `pg_isready` health check to
   report healthy before any job step runs.
2. The service's `POSTGRES_DB` environment variable makes it create the test database itself on
   container startup — no separate "create the database" step.
3. `server/scripts/run-db-tests.sh` runs `tests/db_migrations.rs` first (a dedicated, minimal
   `#[ignore]`d test that does nothing but apply every migration in `server/migrations/` — see
   that file's own doc comment) as an explicit, separately-logged fail-fast checkpoint, then runs
   the rest of the ignored suite (`auth_flow`, `license_flow`, `payment_flow`,
   `reconciliation_flow`, `least_privilege_role`, `ready`, `admin_api_flow`, `db_migrations` again
   as a harmless idempotent re-run) via `cargo test -p license-server --all-targets -- --ignored`.
4. Any failure in either step fails the `db-tests` job (and therefore the whole workflow run) —
   nothing about this job swallows or soft-fails an error.

`-p license-server` scopes this to the licensing/payment server crate only — the desktop crate's
own `#[ignore]`d tests (`tests/import_pipeline.rs`, repository root) document unrelated,
pre-existing PDF-extraction bugs, not Postgres dependencies, and must never be swept up by this
job.

**Environment variables used** (workflow-level only, never hardcoded in Rust source):

| Variable | Set by | Value in CI |
|---|---|---|
| `DATABASE_URL` | `db-tests` job's `env:` | `postgres://postgres:postgres@localhost:5432/license_server_test` — the Postgres *admin* account, since `tests/least_privilege_role.rs` needs to create/alter the restricted `license_server_app` role and `tests/db_migrations.rs`/every other flow test needs to apply migrations. |

**Reproducing locally** — the exact same script CI runs:

```sh
docker run -d --name license-server-test-db \
  -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=license_server_test -p 5432:5432 postgres:16

DATABASE_URL=postgres://postgres:postgres@localhost:5432/license_server_test \
  server/scripts/run-db-tests.sh

docker stop license-server-test-db && docker rm license-server-test-db
```

Or, to run one specific suite by hand exactly as before (still fully supported —
this finding did not change any test's `#[ignore]` attribute or add any new requirement on top of
`DATABASE_URL`):

```sh
DATABASE_URL=postgres://postgres:postgres@localhost:5432/license_server_test \
  cargo test -p license-server --test auth_flow -- --ignored
```
