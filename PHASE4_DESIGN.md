# Phase 4 Design — Licensing Server + Razorpay Payment Gateway

**Status: DRAFT — design only. Awaiting your approval. No Rust code, server code, database
schema, UI, migrations, or project structure have been created or modified for this phase.**

**Scope:** the item `LICENSE_SYSTEM_DESIGN.md` §7 and §9 explicitly deferred — "do not start
Payment Gateway implementation until this licensing architecture is complete and approved."
Phase 3 (commit `5e5503a`) closed that gate. Phase 4 designs the real server behind
`API_SPECIFICATION.md`, Razorpay as the payment provider, and the desktop's real
`HttpLicenseClient` (the existing `LicenseApiClient` trait in `src/license/client.rs` already
anticipates this — see `LICENSE_SYSTEM_DESIGN.md` §9).

**Explicitly not in this phase:** flipping `license::should_enforce()` to `true`. That is a
separate, later, one-line, approval-gated change (`LICENSE_SYSTEM_DESIGN.md` §7's own reasoning)
made only after the full buy → webhook → refresh loop has run clean in production for a while.
Phase 4 makes the machinery real; it does not turn on the lock. See §11 for exactly where that
decision re-enters the picture.

---

## 1. Licensing server architecture

### 1.1 Repository layout

Two independent Cargo projects in one repo, plus one small shared crate for wire types — not a
single unified workspace merging build graphs:

```
bank-statement-processor-rust/
├── Cargo.toml            # existing desktop app — [package], untouched
├── src/                  # existing desktop app — untouched
├── protocol/             # shared DTOs + ApiError enum, no GUI/DB deps
│   └── Cargo.toml
└── server/                # the licensing + payment server
    └── Cargo.toml
```

Root `Cargo.toml` would gain a `[workspace]` table listing `protocol` and `server` (Cargo permits
`[package]` + `[workspace]` in one manifest since 1.71; the root package is implicitly a member).
`src/`, `build.rs`, `.cargo/config.toml`, and every existing path stay exactly where they are.

**Why not one unified workspace:** the desktop app's dependencies (Slint, SQLCipher/rusqlite,
keyring, rfd) and the server's (axum, sqlx, Postgres driver) share almost nothing and have
independent release lifecycles — desktop ships as a signed `.exe` on a slow cadence; the server
deploys to a cloud host on its own schedule. Separate `cargo build` graphs (only `protocol`
shared) mean one side's dependency bump never forces a rebuild/retest of the other.

**Why a shared `protocol` crate instead of duplicating DTOs (today's state):**
`src/license/client.rs` already hand-defines `ActivateLicenseRequest`, `ApiError`, etc., written
to match `API_SPECIFICATION.md` *by convention*, not by shared code. A server built independently
against the same markdown spec can drift from it silently — this project has hit that exact
"looks done but isn't" shape before (dashboard cards, GST fields, OCR overlay never wired — see
project history). Putting `ApiError` and the 7+1 request/response structs in `protocol/`, with
both desktop and server depending on it, turns drift into a compile error instead of a review
miss. `protocol` has zero I/O — pure `serde`-derived structs — so it adds negligible weight to
either side.

### 1.2 Server internal architecture (layered)

```
┌─────────────────────────────────────────────────────────┐
│ HTTP layer (axum routers + tower middleware)             │
│  - rate limiting, request logging, CORS (none needed —   │
│    no browser JS calls this API directly)                │
├─────────────────────────────────────────────────────────┤
│ Auth middleware — bearer token extraction + session      │
│ lookup, injects AuthedUser into handler context           │
├─────────────────────────────────────────────────────────┤
│ Handlers — one per endpoint (§3), thin: parse request,    │
│ call a service function, map Result into envelope+status  │
├─────────────────────────────────────────────────────────┤
│ Services — license.rs, payment.rs, auth.rs: business       │
│ logic (device-limit checks, status derivation, webhook    │
│ processing), independent of HTTP framework types           │
├─────────────────────────────────────────────────────────┤
│ Data access — sqlx queries against Postgres                │
├─────────────────────────────────────────────────────────┤
│ External — Razorpay HTTP client (order/subscription        │
│ creation), reqwest                                          │
└─────────────────────────────────────────────────────────┘
```

Handlers never talk to sqlx or Razorpay directly — keeping business logic in `services/` (not
framework handler bodies) is what makes the integration-test strategy in §9 possible without
spinning up axum for every test.

### 1.3 Framework and dependency choices

| Concern | Choice | Why |
|---|---|---|
| HTTP framework | `axum` | tokio-based; the desktop app already depends on `tokio` for AI HTTP calls, keeping the async story consistent across the repo even though the two binaries never share a process. |
| Database | PostgreSQL via `sqlx` | `LICENSE_DATABASE_SCHEMA.md` §1 is already written in Postgres-flavored SQL (`BIGSERIAL`, `TIMESTAMPTZ`, `INET`) — designed for this, not incidentally compatible. `sqlx` gives async queries + compile-time-checked SQL + a built-in migration runner. |
| Password hashing | `argon2` | current OWASP-recommended KDF for `users.password_hash`; not reusing the desktop's HMAC-based `monthly_password.rs` scheme, which is explicitly documented there as "a licensing gate, not an access-control boundary" — server-side account passwords need real credential-storage hygiene, a different threat model. |
| Session tokens | Random 256-bit, stored **hashed** (SHA-256) in a `sessions` table | `POST /logout` must genuinely invalidate a token per spec — a stateless JWT can't be revoked without a blocklist, which is just a worse-shaped sessions table. Hashing at rest follows the same principle the desktop already applies to its own secrets. |
| Rate limiting | `tower` + `governor` | keyed by `device_id` for license endpoints, IP for `/login` — implements the spec's own `429 RATE_LIMITED` code. |
| Webhook signature | `hmac` + `sha2` | Razorpay signs webhooks HMAC-SHA256; these crates are already a dependency desktop-side (`auth/monthly_password.rs`), so no new crypto crate enters the tree. |
| Razorpay HTTP calls | `reqwest` (async) | Order/Subscription creation, same crate family the desktop already uses (blocking variant) for AI calls. |

---

## 2. Razorpay integration

- **Lifetime plan → Razorpay Orders API.** One-time payment, single capture event.
- **Monthly/yearly plans → Razorpay Subscriptions API.** Recurring billing. A Razorpay Plan is
  created once per `plan_type` out of band (dashboard or a one-time setup script), not per
  purchase — the server only ever creates *Subscriptions* against those pre-existing Plan IDs.
- **Checkout surface:** Razorpay's own hosted checkout page, opened in the user's system default
  browser — not an embedded webview or custom card form. This app is a native Slint desktop app
  with no browser engine; reusing Razorpay's PCI-compliant hosted page is the only integration
  shape that avoids ever handling card data in this codebase.
- **plan_type → Razorpay Plan ID mapping:** a small env-driven config map (3-4 entries, changes
  rarely) rather than a new database table — this is deployment config, not application data.
  Revisit only if/when an admin dashboard needs to manage pricing without a redeploy.

### 2.1 Purchase flow (end to end)

```
Desktop                      Server                         Razorpay
   │  POST /create-checkout-    │                                │
   │  session {plan_type}       │                                │
   │───────────────────────────>│                                │
   │                            │  create Order/Subscription      │
   │                            │─────────────────────────────────>│
   │                            │<─────────────────────────────────│
   │                            │  insert payments (pending)      │
   │  { checkout_url }          │                                │
   │<───────────────────────────│                                │
   │  open checkout_url in      │                                │
   │  system browser            │                                │
   │                             ·                                │
   │                    (user pays in browser, on Razorpay's page) │
   │                             ·                                │
   │                            │<── webhook: payment.captured ───│
   │                            │  verify HMAC signature          │
   │                            │  idempotency check (event_id)   │
   │                            │  update payments → succeeded    │
   │                            │  update subscriptions → active  │
   │                            │  create/extend licenses row     │
   │  "I've paid" → POST        │                                │
   │  /refresh-license          │                                │
   │───────────────────────────>│                                │
   │  { status: active, ... }   │                                │
   │<───────────────────────────│                                │
```

The desktop never learns about payment success from the browser redirect (browser tabs are
disconnected from the desktop process) — it learns via the user clicking "I've completed
payment — Refresh" after returning to the app, which calls the already-spec'd
`/refresh-license`. This is why that endpoint exists as distinct from `/validate-license` per
its own doc comment in `API_SPECIFICATION.md`: to reflect a just-completed payment immediately
rather than waiting for the next natural validation cycle.

---

## 3. API endpoints and request/response flow

The 7 endpoints in `API_SPECIFICATION.md` (`/login`, `/activate-license`, `/validate-license`,
`/refresh-license`, `/logout`, `/subscription`, `/heartbeat`) are implemented exactly to that
spec — no changes to envelope shape, error codes, or the `status` enum. `HttpLicenseClient` was
designed against that document; this server design commits to matching it byte-for-byte via the
shared `protocol` crate (§1.1), not by separately re-reading the markdown.

**One additive endpoint**, required because payment was out of scope when the original 7 were
specified:

```
POST /create-checkout-session
Auth: Bearer <session_token>
Request:  { "plan_type": "monthly" | "yearly" | "lifetime" }
Response: { "ok": true, "data": {
    "checkout_url": "https://checkout.razorpay.com/...",
    "provider_ref": "order_xyz"
} }
Errors: 401 UNAUTHORIZED, 400 INVALID_PLAN_TYPE (new code), 502 PROVIDER_ERROR (new code — Razorpay API call itself failed)
```

**Webhook endpoint** (not part of the customer-facing 7+1 — server-to-server only):

```
POST /webhooks/razorpay
Auth: none (public endpoint) — authenticated via HMAC signature instead, see §4
```

### 3.1 Request/response flow for the two hot paths

**Startup validation** (`/validate-license`, called on every online app launch per
`LICENSE_SYSTEM_DESIGN.md` §4): desktop sends `license_id` + `device_id` + `machine_fingerprint`
+ `client_clock`; server checks `devices` table for that pair, checks `licenses.status`/
`expires_at`, logs the attempt to `license_validation_logs` (§6), returns `status` +
`expires_at` + `grace_period_days` + `server_time` + `fingerprint_matched`. Desktop merges this
into its local `LicenseStatus` derivation exactly as already implemented in
`license::validation` (unchanged by this phase).

**Activation** (`/activate-license`, called once per license+device pair): server looks up
`license_key`, checks `licenses.status` (rejects revoked/expired), checks device count against
`max_devices` (409 if exceeded, response includes the existing device list), inserts a `devices`
row, returns license terms. This is the only endpoint that can *create* a `devices` row.

---

## 4. Webhook verification

1. **Signature check first, before touching the database.** Razorpay sends
   `X-Razorpay-Signature: <hex hmac-sha256>` computed over the *raw* request body using the
   webhook secret (a value configured in the Razorpay dashboard and stored server-side — §7).
   The handler recomputes the HMAC over the raw bytes it received and compares in constant time
   (`subtle`-style comparison, or the constant-time compare already available via the `hmac`
   crate's `verify_slice`). A mismatch → `401`, request discarded, nothing written.
2. **Idempotency check.** Razorpay redelivers on timeout/non-2xx, so the same event can arrive
   more than once. `(provider, event_id)` is looked up in `payment_webhook_events` (§6) before
   any other write; if already present, return `200` immediately and do nothing further.
3. **Only after both checks pass** does the handler update `payments`/`subscriptions`/
   `licenses` state, inside a single database transaction (a webhook that updates `payments` but
   crashes before updating `subscriptions` must not leave inconsistent state — see §5 rollback
   note).
4. Handled event types: `payment.captured`, `payment.failed`, `subscription.activated`,
   `subscription.charged`, `subscription.cancelled`, `subscription.halted`. Unrecognized event
   types are logged and acknowledged with `200` (Razorpay expects a 2xx for events it doesn't
   need retried, even ones this server doesn't act on) rather than treated as an error.

---

## 5. Security model

Extends the ground rule already stated in `LICENSE_SECURITY_REVIEW.md` ("the desktop app is
never trusted... the server is the only party that can *grant* a license"). Phase 4 is where
that server actually starts existing, so its own security posture now matters directly:

- **Transport:** TLS mandatory on every endpoint, including the webhook. No plaintext HTTP
  listener exposed publicly, even during early staging (use a self-signed cert + pinned CA on
  the desktop's staging build if needed, never an unencrypted staging deployment reachable from
  the internet).
- **Session tokens:** random, high-entropy, hashed at rest (§1.3), short-lived (`expires_at`),
  and genuinely revocable via `/logout` (`revoked_at`). Bearer token compromise is scoped to the
  session lifetime, not the account's lifetime.
- **Webhook trust boundary:** the *only* thing that authenticates a webhook call is HMAC
  signature verification (§4) — there is no bearer token on that endpoint, by design (Razorpay
  doesn't send one), so signature verification is not optional hardening, it *is* the auth
  mechanism. A missing or misconfigured webhook secret is a total-compromise-of-payment-integrity
  bug, not a minor gap — flagged again in §11 (Risks).
- **Device/license checks unchanged:** `/activate-license` and `/validate-license` keep the
  device-limit and fingerprint-mismatch handling already designed in
  `LICENSE_SYSTEM_DESIGN.md` §5 and `LICENSE_SECURITY_REVIEW.md` §2/§5 — a fingerprint mismatch
  is logged, not auto-blocked, consistent with the existing documented policy. Phase 4 doesn't
  change that policy, only gives it a real server to run against.
- **Rate limiting:** `/login` limited per-IP (brute force), `/validate-license` and `/heartbeat`
  limited per-`device_id` (already anticipated by the spec's `429 RATE_LIMITED` code) —
  prevents one compromised/buggy client from flooding the validation log table or masking a
  real anomaly-detection signal (`LICENSE_SECURITY_REVIEW.md` §4's clone-detection concern
  depends on validation logs being meaningful, not spammed).
- **Least privilege on the DB role:** the server's Postgres role should have `INSERT`/`SELECT`/
  `UPDATE` on its own tables and nothing else (no `DROP`, no superuser) — a compromised server
  process shouldn't be able to do more damage than the application logic itself already could.
- **Idempotent, transactional webhook processing** (§4) prevents a retried or partially-failed
  webhook from double-charging state changes (e.g., extending `expires_at` twice for one
  renewal).

---

## 6. Authentication and secret management

**Two distinct authentication surfaces, not to be confused:**
1. *Server account* auth (`POST /login`, `users` table) — a customer's billing/license-management
   account. New in this phase.
2. *Desktop app* auth (`auth::validate_credentials`, the existing monthly-password gate) —
   unrelated, unmodified, continues to exist independently exactly as
   `LICENSE_SYSTEM_DESIGN.md` §1 already documented.

**Secrets the server needs**, none of them committed to git (matching this repo's existing
convention of keeping secrets out of source — the desktop's AI API key already lives in the OS
keyring, not in code):

| Secret | Purpose | Storage |
|---|---|---|
| `DATABASE_URL` | Postgres connection string | environment variable, injected by the hosting platform's secret store |
| `RAZORPAY_KEY_ID` / `RAZORPAY_KEY_SECRET` | authenticate server→Razorpay API calls | environment variable |
| `RAZORPAY_WEBHOOK_SECRET` | verify inbound webhook HMAC (§4) | environment variable |
| Session-token generation uses OS RNG, not a static key | n/a — no signing secret needed for random opaque tokens (unlike JWT) | n/a |

No `.env` file committed; local development uses an uncommitted `.env` (already the pattern
implied by `keyring`/`OPENSSL_DIR` usage elsewhere in this repo) or the hosting platform's
secret manager in staging/production. Secret rotation: `RAZORPAY_KEY_SECRET` and
`RAZORPAY_WEBHOOK_SECRET` rotation both just require an env var update + restart — no code
change, no data migration.

---

## 7. Database design

Extends `LICENSE_DATABASE_SCHEMA.md` §1 (already "design only" — nothing here modifies existing,
already-implemented desktop-side migration 6, which is untouched by this phase). Two new tables
that weren't anticipated because payment was out of scope when that document was written:

```sql
CREATE TABLE sessions (
    id            BIGSERIAL PRIMARY KEY,
    user_id       BIGINT NOT NULL REFERENCES users(id),
    token_hash    TEXT NOT NULL UNIQUE,   -- SHA-256 of the bearer token; token itself never stored
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    revoked_at    TIMESTAMPTZ              -- set by POST /logout
);
CREATE INDEX idx_sessions_user ON sessions(user_id);

CREATE TABLE payment_webhook_events (
    id              BIGSERIAL PRIMARY KEY,
    provider        TEXT NOT NULL,          -- 'razorpay'
    event_id        TEXT NOT NULL,          -- provider's own idempotency key
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    processed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, event_id)
);
```

`payments`, `subscriptions`, `licenses`, `devices`, `users`, `login_history`,
`license_validation_logs` are already fully specified in `LICENSE_DATABASE_SCHEMA.md` §1 and
need no schema changes — Phase 4 only starts actually writing rows into `payments` (already
called out in that document as schema-ready for exactly this).

**Migration tooling:** `sqlx migrate` (or `refinery`, both idiomatic for this stack) — a
`server/migrations/` directory of numbered `.sql` files, run automatically on server startup
against `DATABASE_URL` before accepting traffic, mirroring the desktop's own "migration is the
single source of truth for schema shape" philosophy (`db::MIGRATIONS` in `db/mod.rs`) applied to
the server's own, entirely separate, database.

---

## 8. Deployment architecture

**Confirmed:** single VPS, Docker Compose, Caddy for TLS, PostgreSQL in Docker, custom domain
(`license.example.com` used below as a placeholder for whatever domain is actually registered).

```
Internet
   │  HTTPS (443) — only port exposed publicly
   ▼
┌─────────────────────────────────────────────────────────────────┐
│ VPS                                                              │
│                                                                    │
│  ┌───────────────┐   docker network: "licensing"                  │
│  │ caddy          │   (internal, not published to host except 443/80) │
│  │ - :443 (TLS,   │                                                │
│  │   auto Let's   │──────┐                                         │
│  │   Encrypt)     │      │  reverse_proxy license.example.com      │
│  │ - :80 (ACME     │      │  → server:8080                          │
│  │   challenge +   │      ▼                                         │
│  │   http→https)   │  ┌───────────────┐                             │
│  └───────────────┘  │ server (axum)  │  env_file: .env (not in git) │
│                       │  :8080         │──────┐                      │
│                       └───────────────┘      │ DATABASE_URL          │
│                                                ▼                      │
│                                        ┌───────────────┐              │
│                                        │ postgres:16    │              │
│                                        │  :5432 (internal│              │
│                                        │  only — not      │              │
│                                        │  published to    │              │
│                                        │  host)            │              │
│                                        └───────┬───────────┘              │
│                                                │ named volume              │
│                                                ▼                            │
│                                     pgdata (persistent, survives           │
│                                     container recreation)                  │
└─────────────────────────────────────────────────────────────────┘

Razorpay ──webhook (HTTPS, HMAC-signed)──> Caddy (:443) ──> server:8080/webhooks/razorpay
Desktop  ──HttpLicenseClient (HTTPS)──────> Caddy (:443) ──> server:8080/...
```

### 8.1 Docker Compose shape (illustrative — not a file created by this design)

Three services in one `docker-compose.yml`, one Docker network, one named volume:

- **`caddy`** — official `caddy` image. `Caddyfile` (single block):
  ```
  license.example.com {
      reverse_proxy server:8080
  }
  ```
  Caddy handles the ACME HTTP-01 challenge and certificate renewal automatically — no manual
  certbot/cron needed. Only `caddy` publishes ports to the host (`80:80`, `443:443`); `server`
  and `postgres` are reachable only over the internal Docker network, never bound to the host's
  public interface. This means a misconfiguration can't accidentally expose Postgres or the raw
  axum port to the internet — the network topology enforces it, not just a firewall rule someone
  could forget.
- **`server`** — the `server/` crate's release binary, built via a multi-stage Dockerfile
  (build stage with the Rust toolchain, slim runtime stage with just the compiled binary — keeps
  the deployed image small and free of build tooling). Reads all config from environment
  variables (§6) supplied via `env_file: .env` in the compose file, where `.env` lives on the VPS
  only, is `chmod 600`, and is never committed (already covered by this repo's `.gitignore`
  conventions for secrets — extend it to cover this file explicitly). Runs its own migrations
  (§7) on startup before binding the listener, so a fresh deploy is a `docker compose up -d`
  away from a working schema with no separate manual migration step.
- **`postgres`** — official `postgres:16` image, `POSTGRES_PASSWORD`/`POSTGRES_DB` from the same
  `.env`, data directory on a named Docker volume (`pgdata`) so `docker compose down` (without
  `-v`) never loses data, only `docker compose down -v` or explicit volume deletion would — a
  deliberately hard-to-do-by-accident action.

### 8.2 Backups

Since Postgres runs in Docker on a single VPS (no managed-database automatic backups), backups
are the operator's explicit responsibility here — a nightly `pg_dump` (via a small `cron`
entry on the host, or a `postgres`-image sidecar container running `pg_dump` on a schedule)
writing compressed dumps to a path outside the Docker volume (ideally off-VPS — e.g. synced to
object storage), retained on a rolling window of **14 daily backups + 8 weekly backups**
(confirmed, §14 item 10). This is a real gap
versus a managed database and is called out explicitly in §11 (Risks) rather than assumed away.

### 8.3 Operational properties

- **Server is stateless** beyond in-memory rate-limit counters — a `docker compose restart
  server` (e.g. after a config change) doesn't lose committed data, only in-flight requests and
  the rate-limit window, both acceptable.
- **Single VPS is a single point of failure** by construction (no managed-DB failover, no
  multi-node orchestration) — an explicit, accepted trade-off for this stage of the product, not
  an oversight; revisit if/when uptime requirements justify the added operational complexity of
  a managed Postgres + multi-instance server.
- **Health check** — `GET /healthz` (unauthenticated, checks DB connectivity) used by Docker's
  own `healthcheck:` directive so `docker compose ps` and any external uptime monitor can see
  real service health, not just "the process is running."
- **Logging** — `tracing` (axum-idiomatic, distinct from the desktop's simpler `log`/`env_logger`
  pair) to stdout/stderr, captured via Docker's own log driver; a log-shipping sidecar or
  external monitoring agent is a reasonable later addition, not required for Phase 4 itself.
- **Deploys** — a new image build + `docker compose up -d --no-deps server` on the VPS (manually
  or via a simple CI step that SSHes in and runs it); no blue/green or zero-downtime deploy
  machinery designed here — a few seconds of downtime during a `server` container restart is an
  accepted trade-off at this stage, same reasoning as the single-VPS point above.

---

## 9. Testing strategy

- **`server/`:** integration tests against a real ephemeral Postgres (`testcontainers` crate, or
  a docker-compose test DB spun up in CI) covering:
  - Each of the 8 endpoints' happy path + every documented error code.
  - Webhook idempotency — same `event_id` delivered twice results in exactly one state change.
  - Webhook signature rejection — tampered payload or wrong secret → `401`, no DB write.
  - Rate-limit triggering on `/login` and `/validate-license`.
  - Device-limit enforcement (`409 DEVICE_LIMIT_REACHED`) including the returned device list.
  - Because handlers are thin and business logic lives in `services/` (§1.2), most of this can
    run against the service layer directly, with a thinner set of true end-to-end HTTP tests on
    top — not everything needs a running axum instance.
- **`protocol/`:** trivial serde round-trip tests only (it has no logic beyond types) — mainly
  exists to catch a struct/field being edited on one side and not regenerated on the other,
  which a shared crate makes structurally impossible rather than something a test needs to catch.
- **Desktop (`HttpLicenseClient`):** tested against a mock HTTP server (`wiremock` crate, dev
  dependency only) so the error-code-mapping logic is verified without a real network call or a
  real server — mirrors how `OfflineClient`'s existing tests work today, with a stubbed
  transport instead of an immediate `Err`.
- **Manual staging pass before any production cutover:** a real Razorpay *test-mode* purchase
  (Razorpay provides sandbox card numbers for exactly this) run against the staging server,
  observed end-to-end: checkout → webhook received → license row created → desktop
  `/refresh-license` shows `Active`. This is the one thing automated tests can't fully substitute
  for, since it exercises the actual Razorpay integration, not a mock of it.

---

## 10. Rollout and rollback plan

**Rollout is staged, each stage independently reversible:**

1. **Server skeleton, no Razorpay.** Deploy `server/` implementing the 7 spec'd endpoints
   against real Postgres, no payment code live yet. Point a staging build of the desktop app's
   `HttpLicenseClient` at it. Rollback: nothing user-facing depends on this yet — tear down the
   deployment, no data to migrate back.
2. **Razorpay checkout + webhook, staging only, test-mode keys.** Rollback: same — staging-only,
   no production exposure.
3. **Desktop `HttpLicenseClient` + Settings "Buy License" flow, shipped in a build pointed at
   staging.** Verify the full loop manually (§9). Rollback: revert the desktop build to
   `OfflineClient` (a one-line change at the existing call site, §1.4 of the earlier draft —
   `OfflineClient` is kept in the codebase specifically so this reversion stays trivial).
4. **Payment reconciliation job (§12) built and verified in staging, before any production
   exposure.** Verification specifically means deliberately breaking the webhook path in staging
   (e.g. temporarily pointing the Razorpay test-mode webhook URL at nothing) and confirming the
   reconciliation job still detects and heals a completed-but-unwebhooked test payment on its
   next run. Rollback: the job runs read-mostly against Razorpay + writes only through the same
   idempotent path webhooks use (§12) — disabling it (stop the scheduled task/container) is
   always safe and never leaves state worse than "webhook-only" would.
5. **Cut server over to production Razorpay keys, real database.** This is the first stage with
   real money and real customer data. Rollback plan specifically for this stage:
   - Database: nightly `pg_dump` backups (§8.2 — this VPS has no managed-database automatic
     backup, so this is a manual/cron responsibility, not assumed) taken before cutover and on
     the regular schedule after.
   - Application: keep the previous server image tag deployable via one command — if a bug in
     the new server corrupts `licenses`/`payments` state, roll the *server* back first, then
     restore the *database* from the pre-bug backup if the corruption already happened, rather
     than trying to hand-patch rows live.
   - Razorpay itself is the source of truth for whether money actually moved — the `payments`
     table is a mirror of Razorpay's own records (via `provider_ref`), so a database rollback
     never risks losing track of a real payment; the reconciliation job (§12) is exactly what
     re-syncs local state against Razorpay after such a restore.
6. **Ship the production-pointed desktop build.** This is the stage every real customer sees.
   Rollback: same as step 3 — revert to `OfflineClient` in a follow-up build, which degrades
   gracefully (the app already handles "no server configured" as an honest message, per Phase 3).

**What is never part of this rollout:** flipping `license::should_enforce()`. That happens, if
at all, as its own separate decision after step 6 has been stable in production — see §11.

---

## 11. Risks and open concerns

| Risk | Impact | Mitigation in this design |
|---|---|---|
| Webhook secret misconfigured or leaked | Forged webhook could fabricate a "payment succeeded" event and grant a free license | HMAC verification is the sole auth mechanism (§4/§5) — treat this secret with the same care as a database credential; rotate immediately if ever suspected leaked |
| Webhook delivery failure (Razorpay retries, or a payment succeeds but the webhook never arrives) | Customer pays, license never activates | `/refresh-license` gives the desktop a manual "try again" path independent of webhook timing; the reconciliation job (§12) is the systematic fix, built and verified in staging before production rollout (§10 stage 4) |
| Double-processing a webhook | Duplicate `expires_at` extension, incorrect license terms | Idempotency table + transactional update (§4), same idempotency key reused by the reconciliation job (§12) so the two paths can never double-apply the same payment |
| Razorpay API outage during checkout creation | Customer can't start a purchase | `502 PROVIDER_ERROR` surfaced honestly to the desktop (matching this codebase's existing "no server configured" honesty precedent from Phase 3) rather than a fake success |
| Clock skew between server and Razorpay/desktop | Subtle expiry-timing bugs | `client_clock` logging already designed in `LICENSE_SYSTEM_DESIGN.md`/`API_SPECIFICATION.md`; server uses its own clock as authoritative, never trusts client-reported time for actual expiry decisions |
| Scope creep into flipping `should_enforce()` alongside this work | Every existing installation with no license suddenly locked out, conflated with an unrelated infrastructure change | Explicitly out of scope for Phase 4 (stated at the top of this document and in §10) — a separate, later, single-line change requiring its own approval |
| Database becomes a second source of truth that drifts from Razorpay's own records | Support disputes ("I paid but app says unlicensed") become hard to resolve | `payments.provider_ref` keeps every local row traceable back to the exact Razorpay object; the reconciliation job (§12) actively re-syncs against it on a schedule, not just on request |
| Single VPS, self-managed Postgres — no automatic failover or managed backups | A VPS failure or disk corruption can lose license/payment data | §8.2's nightly `pg_dump` to off-VPS storage is the mitigation; still weaker than a managed database's point-in-time recovery — an accepted trade-off for this stage, revisit if uptime/data-durability requirements grow |

---

## 12. Payment reconciliation job

**Confirmed in scope for Phase 4, built after the webhook flow (§4) and verified in staging
before any production rollout (§10 stage 4).** Purpose: webhooks are Razorpay's best-effort
push notification, not a guarantee — this job is the pull-based backstop that makes "the local
database eventually matches Razorpay's own records" true even if a webhook is lost, delayed
past its retry window, or arrives while the server is down.

### 12.1 How it runs

A scheduled task inside the `server` process itself (a `tokio::time::interval` background task
spawned alongside the axum listener at startup) rather than a separate container/cron job —
keeps deployment simple (one image, one process, §8's compose file doesn't need a fourth
service) and gives it direct access to the same service-layer functions (`services/payment.rs`)
the webhook handler already uses, so there is exactly one code path that mutates payment/license
state from a Razorpay event, called from two triggers (webhook push, reconciliation pull).
Runs on a fixed interval of **15 minutes** (confirmed, §14 item 8) — frequent enough that a
lost webhook is caught well within any reasonable customer-support SLA, infrequent enough to
stay well clear of Razorpay's API rate limits.

### 12.2 What it checks, each run

1. Query Razorpay's API for payments/orders/subscription events in a trailing window of
   **2 hours** (confirmed, §14 item 9) — generously overlapping runs, so a single missed run
   doesn't create a gap; Razorpay's
   own APIs support listing payments by time range).
2. For each Razorpay payment/event returned, check whether `payment_webhook_events` already has a
   matching `(provider, event_id)` row.
   - **Already present:** webhook already handled it — skip, nothing to do. This is the common
     case on every run once the system is healthy.
   - **Missing locally but Razorpay shows it captured/succeeded:** this is exactly the "webhook
     never arrived" gap. The job calls the **same** `services/payment.rs` function the webhook
     handler calls (§4 step 3 — update `payments`/`subscriptions`, create/extend `licenses`),
     using the Razorpay event's own id as the idempotency key, then inserts the
     `payment_webhook_events` row itself (so a webhook that arrives *later* for the same event,
     e.g. a very late retry, is now the one that's skipped as a duplicate — the two paths are
     fully symmetric and order-independent).
3. Every reconciliation run — including runs that find nothing to heal — writes one summary log
   line (checked count, healed count, window) for observability; a run that heals anything also
   logs the specific `event_id`/`license_id` healed, at a higher log level, since that's the
   signal an operator actually wants to notice.

### 12.3 What it deliberately does not do

- **No automatic refund or cancellation logic** — if Razorpay shows a payment as `failed` or
  `refunded` that the local `payments` row still shows as `pending`, the job updates the local
  status to match (that's still "sync local state to Razorpay's truth"), but it never initiates
  a refund or subscription cancellation *itself* — those are actions on money, not just
  observations of it, and stay a manual/admin action even after Phase 4.
- **No silent healing of ambiguous cases** — if a Razorpay payment references a
  `subscription_id`/`license_key` combination the local database has no record of at all (not
  just a missing webhook, but a genuinely unknown reference — e.g. a payment created by a
  process outside this server, or corrupted local data), the job logs it as an anomaly and does
  **not** guess; that is a support/investigation case, not an auto-heal case, consistent with
  this codebase's established fail-closed posture (`LICENSE_SECURITY_REVIEW.md` §6 — no
  catch-all "else → grant access" branch anywhere in the licensing system, and this job doesn't
  introduce the first one).

### 12.4 Testing (extends §9)

- Integration test: seed a `payments` row absent from `payment_webhook_events`, mock the
  Razorpay list-payments API response to include it as `captured`, run the reconciliation
  function once, assert the license was created/extended and the event row now exists.
- Integration test: run reconciliation twice over the same mocked Razorpay response, assert the
  second run makes zero additional writes (idempotency).
- Staging verification per §10 stage 4's specific deliberate-break scenario.

---

## 13. Implementation phases (once this design is approved)

1. `protocol/` crate — shared DTOs + `ApiError`, extracted from today's `src/license/client.rs`
   without behavior change.
2. `server/` skeleton — axum + sqlx wiring, `sessions`/`payment_webhook_events` migrations, the
   7 spec'd endpoints implemented against Postgres, no Razorpay yet. Deployed to the VPS via
   Docker Compose + Caddy (§8) in a staging configuration.
3. Razorpay integration — `/create-checkout-session` + `/webhooks/razorpay`, test-mode keys,
   staging deployment.
4. Desktop `HttpLicenseClient` + Settings "Buy License" / "Refresh" UI, pointed at staging.
5. Payment reconciliation job (§12), verified in staging per §10 stage 4's deliberate-break test.
6. End-to-end manual verification in staging (§9), then production cutover per the staged
   rollout in §10 (production Razorpay keys, production desktop build).

Each phase above should get its own explicit go-ahead before starting, consistent with how this
project has run every prior phase — this document is asking for approval of the *design*, not a
blanket green light to run all six implementation phases unattended.

---

## 14. Hosting and scope decisions (confirmed)

Resolves what was listed as open questions in the prior draft:

1. **Hosting:** single VPS, Docker Compose. Not a PaaS (Fly.io explicitly ruled out) — §8 designed
   accordingly.
2. **Reverse proxy / TLS:** Caddy, automatic Let's Encrypt certificates.
3. **Database:** PostgreSQL in Docker on the same VPS (not a managed database service) — §8.2's
   self-managed backup responsibility follows directly from this choice.
4. **Domain:** a custom domain will be registered/pointed at the VPS (`license.example.com` used
   throughout this document as a placeholder for the real domain, TBD).
5. **Razorpay:** test-mode keys first (§10 stages 2-4), production keys only after staging
   verification (§10 stage 5) — matches the design's existing test-mode/production-mode
   separation in §6's secret table (same variable names, different values per environment).
6. **Repository layout:** confirmed — §1.1's two-project-plus-shared-protocol-crate layout,
   same repository, no change.
7. **Reconciliation job:** confirmed in scope for Phase 4 — designed in §12, sequenced after the
   webhook flow and before production rollout in both §10 (rollout plan) and §13 (implementation
   phases).
8. **Reconciliation interval:** 15 minutes (§12.1) — fixed value, not a placeholder.
9. **Reconciliation lookback window:** 2 hours per run (§12.2 step 1) — fixed value, not a
   placeholder. Deliberately wider than the 15-minute interval so consecutive runs overlap and a
   single missed/slow run cannot open a real gap.
10. **PostgreSQL backup retention:** 14 daily backups + 8 weekly backups (§8.2), nightly
    `pg_dump`, stored off-VPS.
11. **Reconciliation idempotency and fail-closed behavior (§12.2-12.3):** confirmed as designed,
    no change — same idempotency key/table the webhook path uses, so webhook and reconciliation
    can run in either order without double-applying a payment; any Razorpay reference the local
    database has no record of at all is logged as an anomaly for manual review, never guessed at.
12. **No auto-refund / no auto-correction of ambiguous payment states (§12.3):** confirmed as
    designed, no change — the job syncs local status to Razorpay's own recorded status (e.g. a
    payment Razorpay shows as `failed`/`refunded` updates the local row to match) but never
    itself initiates a refund, cancellation, or any other state change on money; those stay a
    manual/admin action.

**No remaining open questions for Phase 4's design.**
