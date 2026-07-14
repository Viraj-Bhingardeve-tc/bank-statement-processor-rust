# Final Production Readiness Audit

**Repository:** bank-statement-processor-rust
**Branch / commit audited:** `main` @ `e50a7b6`
**Audit type:** Read-only, full-repository production readiness audit (no code modified)
**Scope:** Desktop application (`src/**`, `ui/**`), licensing + payment server (`server/src/**`), shared protocol crate (`protocol/`), database migrations (`server/migrations/*.sql`), deployment tooling (`server/deploy/*.sh`, `Dockerfile`, `docker-compose.yml`, `Caddyfile`), all root/`docs/` documentation, all test suites, and repository hygiene.

---

# Executive Summary

This audit covers security, Rust code quality, the desktop application, the licensing server, the payment gateway (Razorpay), PostgreSQL, deployment, Docker, backup, restore, monitoring, testing, and documentation.

The security and payment *primitives* in this codebase — password hashing, session token generation, webhook HMAC verification, parameterized SQL, transactional webhook idempotency, and secret redaction — are genuinely well-engineered, well above the bar typically seen on a first production pass. Module wiring is also clean: every ported desktop engine (GST, Tally grouping, narration cleaner, reconciliation, classifier) is verifiably reachable from the real application pipeline, not dead code left over from the parity rewrite.

However, four **Critical** issues each independently block a production launch: the desktop application currently has no working license-enforcement gate; one entire Razorpay purchase path (Lifetime/Trial via Payment Links) can silently take a customer's payment and never issue a license, with no automated recovery; refunds and chargebacks never revoke a subscription's license; and database backups exist in a single, unencrypted, on-VPS location with no off-site copy. Layered on top of these: there is no CI/CD pipeline anywhere in the repository, so the Postgres-backed test suites that actually prove the payment and auth systems work correctly are never run automatically; a real crash-causing bug sits on the parsing hot path; and the documentation set actively contradicts the current state of the system in ways that would mislead an engineer or auditor doing due diligence.

The system is architecturally close to production-ready but is not safe to deploy as a paid product in its current state.

---

# Critical

| # | Finding | Domain |
|---|---|---|
| 1 | `src/license/mod.rs:31` — `should_enforce()` is hard-coded `false`. Desktop license validation is fully built (offline grace, clock-rollback watermark, fail-closed rules) but **enforces nothing** — the app runs fully featured with no valid license, no server, and no payment. Confirmed intentional-for-now via in-code comments, but it means there is currently no monetization gate at all. | Desktop / Licensing |
| 2 | `server/src/razorpay/client.rs:278-281` — Lifetime/Trial checkout uses Razorpay **Payment Links** and stores `provider_ref = parsed.id` (a `plink_...` id). `server/src/razorpay/webhook.rs:29-36` (`extract_entity_ref`) and `payment_service.rs:403-406` only ever match against `payment.entity.order_id`/`.id`, never the payment-link id. **Confirmed by direct code read.** Failure: a customer completes a real Lifetime purchase, the webhook can never match the stored ref, `resolve_activation` logs "unknown payment; ignoring", no license is issued, and the 15-min reconciliation job uses the same broken key so it can never self-heal. Real revenue loss with silent failure. | Payment Gateway |
| 3 | `payment_service.rs:163-179` — no handler for `refund.created`/`refund.processed`/`payment.dispute.created`; falls into the ignored catch-all. A refunded or charged-back payment never revokes the subscription/license — `PaymentStatus::Refunded` exists in the schema but is unreachable from any webhook path. No test exercises this anywhere. | Payment Gateway |
| 4 | `server/deploy/backup.sh:36,111-112` — backups are unencrypted `.sql.gz` written only to `/var/backups/license-server` on the same VPS as live Postgres; off-VPS sync (`rclone`) is a commented-out TODO. Disk loss, host compromise, or ransomware destroys/exposes the live DB **and** every backup (password hashes, emails, payment refs) simultaneously — no second copy exists anywhere. | Backup/Restore, PostgreSQL |

---

# High

| # | Finding | Domain |
|---|---|---|
| 5 | `rate_limit.rs:119-128` — `/login`'s rate limiter keys on `ConnectInfo` peer IP. Behind the project's own documented Caddy topology, that's always Caddy's container IP for every request — the 5-req/min budget collapses into one global bucket for all users. Explicitly documented in-code as a known, unfixed caveat. | Security / Licensing server |
| 6 | `rate_limit.rs:151,175` — `device_rate_limit` keys on the fully attacker-controlled `device_id` field with no proof of prior registration, and the keyed limiter is never pruned — rotating `device_id` bypasses flood protection **and** leaks memory indefinitely. | Security |
| 7 | `rate_limit.rs:151` — `axum::body::to_bytes(body, usize::MAX)` with no `DefaultBodyLimit`/`RequestBodyLimitLayer` anywhere — an oversized POST to `/validate-license`/`/heartbeat` is fully buffered into memory (memory-exhaustion DoS). | Security |
| 8 | `routes/error.rs:162` (licensing) and `repository/error.rs:16-23` + `routes/error.rs:112-134` (payment) — raw `sqlx`/DB error text is placed verbatim into JSON 500 bodies returned to callers, leaking schema/constraint detail. | Security (both server domains) |
| 9 | `payment_service.rs:238-272` — subscription suspension/cancellation (`subscription.halted`, etc.) updates `subscriptions.status` only; `licenses` row is never touched, and `license_service::validate/heartbeat` never cross-checks subscription state. A lapsed/failed-payment subscriber keeps a fully valid license for the remainder of the original term. | Payment Gateway |
| 10 | `payment_service.rs:281-330` (`resolve_activation`) — no check of the webhook's actual captured amount/currency against the stored `payments.amount_minor`; a partial capture or manipulated amount still grants full entitlement. | Payment Gateway |
| 11 | No CI/CD exists anywhere (`.github` absent, no other CI config). **All** Postgres-backed tests that prove the critical properties — `auth_flow`, `license_flow`, `payment_flow` (incl. the concurrent-webhook race test), `reconciliation_flow`, `rate_limit_flow`, `least_privilege_role`, `ready` — are `#[ignore]`d and require a manually-supplied `DATABASE_URL`. Only mock-level unit tests run by default; a regression in atomicity, reconciliation, or auth could ship fully green. | Testing, Deployment |
| 12 | `docker-compose.yml` — first-deploy bootstrap is a 5-step manual sequence (start Postgres → migrations create app role via superuser → run `set-app-db-password.sh` → switch `DATABASE_URL` → restart) but `.env.example` already ships `DATABASE_URL` pointed at the restricted role. An operator following the example naively hits a chicken-and-egg failure on the very first deploy. | Deployment, PostgreSQL |
| 13 | `server/deploy/test-backup-restore.sh` is referenced only from `server/README.md` — no cron/Makefile/CI invokes it. A later edit to `backup.sh`/`restore.sh`, or Postgres/psql/gzip version drift, silently breaks disaster recovery until an actual incident. | Backup/Restore |
| 14 | `src/parser/excel_parser.rs` (multiple sites) and `src/ai_classifier.rs:201/238/275` — repeated `&s[..s.len().min(N)]` byte-index slicing, not UTF-8-boundary-safe, on real parsed narration/error text. Will panic on any multi-byte character (₹, accented names, OCR noise) at the cut point in production data. | Rust code quality |
| 15 | `src/main.rs` (~146 call sites) — `.lock().unwrap()` on shared app-state `Mutex` everywhere, with zero `catch_unwind` in the codebase. Any panic while the lock is held (e.g. finding #14) poisons it, bricking the whole app until restart. | Rust code quality |
| 16 | `src/main.rs:950` — `db::save_import(...).ok()` silently swallows DB write failures with no log/toast, unlike the sibling `upsert_transactions` error path a few lines later — invisible data loss for the user. | Rust code quality |
| 17 | `src/db/encryption.rs:236` — the plaintext `.bak` produced during SQLCipher migration is never deleted, leaving a permanent unencrypted copy of all client/transaction data beside the encrypted DB — defeats encryption at rest. | Desktop / Security |
| 18 | `API_SPECIFICATION.md:3`, `LICENSE_SYSTEM_DESIGN.md:3`, `LICENSE_DATABASE_SCHEMA.md:13` — all three still claim "no server exists / specification only," but `server/` is a fully implemented, tested, Dockerized service exceeding the spec. Anyone integrating from "the spec" also misses 6 live endpoints (`/deactivate-license`, `/create-checkout-session`, `/webhooks/razorpay`, `/healthz`, `/readyz`, `/metrics`). | Documentation |
| 19 | `tests/import_pipeline.rs:48-243` — 6 of 11 real bank PDF fixtures (BOB, ICICI, ICICI Wealth Mgmt, IDFC First, Union Bank, Cosmos Co-op) currently parse to **zero transactions**, covered only by `#[ignore]`d tests, not real regression protection in default `cargo test`. This is core product functionality, not an edge case. | Testing |
| 20 | No root `README.md` for the desktop crate — only `server/README.md` exists; no single onboarding doc for building/running the Slint app. | Documentation |

---

# Medium

| # | Finding | Domain |
|---|---|---|
| 21 | `routes/license.rs:103` — `/validate-license`, `/heartbeat`, `/deactivate-license`, `/refresh-license` have no session/ownership check; only credential is a sequential, enumerable `license_id` + `device_id`. Materially weaker boundary than `/subscription`'s `require_session`. | Security |
| 22 | `LICENSE_DATABASE_SCHEMA.md:96` documents `login_history`/`license_validation_logs` tables that don't exist in any migration; `LICENSE_SECURITY_REVIEW.md` relies on them for anomaly/clone detection — a real monitoring gap the docs imply is covered but isn't. | Documentation, Monitoring |
| 23 | `server/migrations/0003_least_privilege_app_role.sql:137` — restricted role gets `CREATE` but never ownership/`ALTER` rights on pre-existing tables; a future non-`CREATE TABLE` migration would fail under this role, untested. | PostgreSQL |
| 24 | `migrations/0002...sql:11-22` — `payments.provider_ref` has only a non-unique index; `find_by_provider_ref` silently picks the most recent match on collision instead of erroring. | Payment Gateway, PostgreSQL |
| 25 | `payment_service.rs:163-179` — hardcoded 6-event allowlist; any other real Razorpay event (e.g. `order.paid`) is silently acknowledged and dropped with only an info log. | Payment Gateway |
| 26 | `payment_service.rs:94-141` — `create_checkout_session` can leave an orphaned `pending_payment` subscription row if the Razorpay call/payment insert fails after the subscription insert; no cleanup job. | Payment Gateway |
| 27 | `routes/metrics.rs:1-9`, `Caddyfile:19` — `/metrics` is publicly exposed with no auth or path restriction, deliberately but riskily. | Monitoring, Security |
| 28 | `Caddyfile:8-20` — no `Strict-Transport-Security`, `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, or CSP anywhere. | Deployment |
| 29 | `routes/ready.rs:44` — `/readyz` returns raw `sqlx::Error` text on an unauthenticated public endpoint. | Monitoring, Security |
| 30 | `docker-compose.yml:80-97` — only Postgres has a healthcheck; Caddy's `depends_on: [license-server]` has no `condition: service_healthy`. A hung-but-listening process is never auto-restarted. | Docker |
| 31 | `src/auth/monthly_password.rs:28` — desktop login is a client-side HMAC keyed by a compiled-in secret, trivially computable offline. Documented/accepted as an anti-piracy nudge, not real access control — flag so it isn't mistaken for a security boundary. | Desktop / Security |
| 32 | `PARITY_GAP_REPORT.md` contains claims ("Narration Cleaner/Tally Group Engine not ported") flatly contradicted by `src/narration_cleaner.rs`, `src/tally_group_engine.rs`; already disowned by two later in-repo audits but not retracted, still misleading new readers. | Documentation |
| 33 | `docs/PRODUCTION_READINESS_AUDIT_2026-06-22.md` — headline "42/100" score mixes now-fixed findings (SQLCipher, migrations) with still-true ones (dead deps) with no addendum distinguishing them. | Documentation |
| 34 | `PROJECT_AUDIT_2026-07-06.md` — "zero integration tests / 15% documentation" is stale; both were added afterward per `PHASE2_COMPLETION_REPORT.md`. | Documentation |
| 35 | `src/parser/ocr_extractor.rs` — the only parser module with zero unit tests; the real Tesseract shell-out path is untested anywhere. | Testing |

---

# Low

| # | Finding | Domain |
|---|---|---|
| 36 | `server/migrations/*` are forward-only with no rollback path beyond a full `restore.sh` (acceptable today since none are destructive, but no tooling exists for a bad-migration rollback short of losing writes since last backup). | PostgreSQL |
| 37 | `server/Dockerfile:13,41,47-48` — base images (`rust:1.90-slim-bookworm`, `debian:bookworm-slim`) and `apt-get install` are tag-only, not digest-pinned; `apt-get update` floats. | Docker |
| 38 | `server/src/observability.rs`, `main.rs:18-27` — Prometheus metrics exist, but tracing is stdout-only; no OTel export or log aggregation off-box. | Monitoring |
| 39 | `reconciliation.rs:22`, `payment_service.rs:475` — fixed 2-hour lookback vs 15-min interval has no catch-up mechanism if the process is down longer than 2 hours; no persisted "last successful run" watermark. | Payment Gateway |
| 40 | `razorpay/client.rs:110-121,328-345` — `list_payments_since` has no pagination (100-item cap); more than 100 payments in the lookback window are invisible to reconciliation. | Payment Gateway |
| 41 | `parity_audit_backup.patch` (62KB, UTF-16LE, doesn't even apply) and `.patch_a_ai_classifier.diff` (already-applied one-liner) — stale artifacts cluttering repo root, already flagged by a prior audit and still not removed. | Repo hygiene |
| 42 | `ui/main_screen.slint.tmp` — tracked in git, contains only the literal text "read"; accidental commit. | Repo hygiene |
| 43 | `src/bin/pdf_diag.rs:5` — hardcoded developer path (`C:\Users\ADMIN\...`); harmless (existence-guarded) but shouldn't ship in a shared diagnostic tool. | Rust code quality |
| 44 | `PARITY_AUDIT_2026-06-24.md` is itself now ~3 weeks stale relative to later Phase 2-4 work, with no superseding note — ironic given it criticizes `PARITY_GAP_REPORT.md` for the same problem. | Documentation |

---

# Nice to Have

| # | Finding | Domain |
|---|---|---|
| 45 | `routes/payment.rs:71-77` — fallback webhook idempotency key (SHA-256 of raw body when `X-Razorpay-Event-Id` is absent) is safe in practice but worth confirming Razorpay always sends that header for every event type in production. | Payment Gateway |

---

## What's Genuinely Solid (for balance)

- **Auth fundamentals:** Argon2id password hashing with correct params, 256-bit CSPRNG session tokens (SHA-256-hashed at rest, raw token never stored), a real fixed timing side-channel with dedicated tests, HMAC-SHA256 webhook verification via constant-time `Mac::verify_slice` against the untouched raw body.
- **SQL:** Every query across both crates is parameterized (`sqlx` `$1`/`.bind`) — no injectable string-built queries found anywhere.
- **Secrets:** `Secret<T>` wrapper correctly redacts `Debug` output everywhere; `.env.example` contains only placeholders; config fails closed on missing/malformed required secrets.
- **Payment atomicity:** `claim_and_apply`'s claim-then-mutate is a single transaction gated by a real unique constraint, closing the documented concurrent-webhook race (proven by a dedicated test); all money fields are integer minor units end-to-end, no floats.
- **Docker:** proper multi-stage build, non-root user, minimal runtime image, no secrets baked into layers; only Caddy exposes host ports.
- **Backup script mechanics:** atomic temp-file + gzip-integrity-check pattern in `backup.sh`; `restore.sh` has explicit confirmation, stops the app first, `ON_ERROR_STOP=1` — good script hygiene even though the storage topology (Critical #4) is the real gap.
- **Module wiring:** every desktop engine (GST, Tally grouping, narration cleaner, reconciliation, classifier) is genuinely reachable from `main.rs`'s real pipeline — no dead-code drift found this pass.
- **Rust discipline:** ~157 `.unwrap()` sites reviewed in the desktop crate; all but the UTF-8 slicing class (High #14) are provably safe/guarded.
- **Test quality where it exists:** GST/Tally/reconciliation/dedup tests assert real bank-specific values against real fixture files through the actual pipeline, not tautological mocks.

---

# Production Readiness Score

## **56 / 100**

The security and payment *primitives* (hashing, tokens, HMAC, parameterized SQL, transactional idempotency, secret redaction) are genuinely well-engineered — better than most first-production-pass code. But four **Critical** issues each independently block a real launch: the desktop app has no working monetization gate, one entire purchase path (Lifetime/Trial) can silently take a customer's money and never deliver a license with no automated recovery, refunds/chargebacks never revoke access, and there is a single unencrypted copy of all backups. Layered on top: zero CI/CD means none of the tests that actually prove the payment/auth system works run automatically, a real desktop crash bug sits on the parsing hot path, and the documentation set actively contradicts the current state of the system in ways that would mislead anyone (including future auditors) doing due diligence.

---

# Final Recommendation

## Can this be deployed to production?

## **NO.**

Fix, at minimum, before launch:

1. The Payment Links `provider_ref` mismatch (Critical #2)
2. Refund/chargeback handling (Critical #3)
3. Off-site/encrypted backups (Critical #4)
4. Subscription-cancellation → license propagation (High #9)
5. The UTF-8 slicing panic (High #14)
6. Wire the ignored Postgres-backed test suite into real CI (High #11)
7. A decision on when `should_enforce()` flips to `true` (Critical #1) — until it does, this is not a licensed/paid product technically, regardless of how complete the payment server is.
