# Final Phase 4L.1 Production Validation Report

**Repository:** bank-statement-processor-rust
**Branch / commit validated:** `main` @ `146f7e2` (clean working tree)
**Phase:** 4L.1 — End-to-End Production Validation
**Audit type:** Read-only synthesis of five parallel workstreams (Desktop, Licensing Server Core, Payment Gateway, Database & Deployment, Cross-Cutting Security & Test Coverage), re-verified against current code. No code modified, nothing committed.
**Supersedes:** `FINAL_PRODUCTION_READINESS_AUDIT.md` (audited at commit `e50a7b6`, now 5 commits behind: `66d2b95`, `b16fca6`, `934ba0b`, `0aa844f`, `146f7e2`)

---

> **STATUS UPDATE — 2026-07-19 (current `HEAD` = `85fff711`, now 9 commits ahead of the `146f7e2` this report audited):** This report's own §1 "Production Ready Items" findings on desktop license enforcement, the server crate, and the protocol crate all **remain valid and re-confirmed** as of current `HEAD` — `should_enforce()` still returns `true`, the real server and its endpoint set are unchanged in that respect, and `protocol/` continues to supply the shared request/response types both sides build against.
>
> Commit history since `146f7e2` (`caf5372`, `38a9b29`, `beb7ef7`, `0aa844f`, `99af102`, `0f2d4e4`, `128d679`, `82b24c8`, `78a9995`, `85fff711`) includes work with plausible bearing on two open items below, **neither independently re-verified line-by-line as part of this documentation-only update**:
> - **H-1 (UTF-8 boundary panic risk):** commit `38a9b29`, "fix(core): harden UTF-8 handling against production crashes," appears to target exactly this issue.
> - **H-4 (No CI/CD):** commit `0f2d4e4` added a GitHub Actions workflow (`fmt`/`clippy`/`test`, workspace-wide) — the CI-doesn't-exist half of H-4 is resolved. The other half is **not**: the workflow does not provision a Postgres service, so the Postgres-backed integration tests this finding specifically called out remain `#[ignore]`d and still do not run automatically on merge.
>
> No other item in §2–§5 (the Critical backup-encryption gap, the remaining eleven High items, or any Medium/Low item) was re-checked against current `HEAD` as part of this update. **The 68/100 score and the No-Go recommendation in §7–§8 should be treated as accurate for `146f7e2`, not as a current assessment** — a fresh full re-validation pass (the same five-workstream method this report used) is the appropriate way to get a current score, not an assumption that time-based progress closed the remaining items.

---

# 1. Production Ready Items

| Item | Evidence | Status |
|---|---|---|
| Desktop license enforcement gate | `src/license/mod.rs:36-38` `should_enforce()` now returns `true` unconditionally; `enforce()` called at login (`main.rs:1664`), activation (`main.rs:3810`), and a 24h periodic timer (`main.rs:4792`); `ui/app.slint:313` conditionally instantiates `MainScreen` only when `logged_in` — a real gate, not a visual hide. 55/55 `license::` unit tests pass. | **FIXED** (was Critical) |
| Payment Links provider-reference correlation | `payment_link.paid` now handled in `payment_service.rs:262-273` via `extract_entity_ref(payload, "payment_link")`, matching the `plink_...` id stored as `provider_ref` at checkout. Full path traced checkout → webhook → `resolve_payment_link_paid` → `find_by_provider_ref` → `resolve_activation`. | **FIXED** (was Critical) |
| Refund / chargeback → license revocation | `refund.created/.processed`, `payment.dispute.created/.closed` handled in `payment_service.rs:444-530` via the new `payments.gateway_payment_id` column (migration `0004`). `resolve_refund` revokes the license; `resolve_dispute_created` suspends; `resolve_dispute_closed` restores or revokes per Razorpay's own dispute status. 10+ dedicated tests including duplicate-event idempotency. | **FIXED** (was Critical) |
| Login timing side-channel | `service/auth_service.rs:76-106` — `verify_password` runs unconditionally against a real or `DUMMY_PASSWORD_HASH` constant; dedicated test proves the dummy hash never authenticates. | **CONFIRMED FIXED** |
| Offline grace period + clock-rollback protection (desktop) | `src/license/validation.rs:58-93` day-based grace window; monotonic `highest_seen_clock` watermark (`storage.rs:188-201`) fails closed on rollback. | **VERIFIED, well-tested** |
| Local license-cache tamper detection (desktop) | `storage.rs:36-66` — HMAC-signed `local_license` row, fails closed on mismatch (migration 7, `integrity_hmac` column). | **VERIFIED** (defense-in-depth; key is compiled-in, see §4 M-9) |
| Reconciliation hardening (Phase 4K.4) | `RECONCILIATION_INTERVAL_SECS`/`BATCH_SIZE`/`MAX_AGE_HOURS` env-configurable, `RazorpayError::Transient`/`Permanent` classification wired end-to-end, defaults unchanged, zero migrations touched. | **VERIFIED correct, no regressions** |
| Device-activation race | `repository/device.rs:174-229` — `SELECT ... FOR UPDATE` on both `licenses` and `devices` inside one transaction, explicit commit/rollback. | **VERIFIED — genuine DB-level fix, not app-level only** |
| Webhook idempotency race | `repository/payment_webhook_event.rs:162-198` — `INSERT ... ON CONFLICT (provider, event_id) DO NOTHING` as the first statement in the mutation transaction, backed by a real `UNIQUE` constraint. | **VERIFIED — closed, proven by concurrent test** |
| SQL injection | Zero `format!`-into-query sites in `server/src`; desktop crate's `format!` usage is table-name-only from fixed/introspected sources, never external input; all row values parameterized. | **VERIFIED — no injectable site in either crate** |
| Webhook spoofing | `auth/webhook_signature.rs:21-34` — constant-time `Mac::verify_slice` over the raw, unmodified request body. | **VERIFIED SECURE** |
| Payment replay | Reconciliation reuses `process_webhook_event` with a stable synthetic event id, hitting the same idempotency claim as a real webhook. No double-issuance path found. | **VERIFIED SECURE** |
| Auth/session fundamentals | Argon2id password hashing, 256-bit CSPRNG session tokens (SHA-256-hashed at rest), `Secret<T>` redaction throughout, fail-closed config. | **VERIFIED SOLID** |
| `/metrics` unauthenticated exposure | `server/README.md:264-311` now explicitly documents this as a deliberate, no-secrets design choice. | **DOWNGRADED from open finding to documented tradeoff** |

---

# 2. Critical Issues Remaining

### C-1. Backups are a single, unencrypted, on-VPS copy
- **Evidence:** `server/deploy/backup.sh:36,110-112` — `BACKUP_DIR=/var/backups/license-server` on the same VPS as live Postgres; output is plain `.sql.gz`; off-VPS sync (`rclone`) is a commented-out `TODO`.
- **Risk:** Disk loss, host compromise, or ransomware destroys or exposes the live database **and** every backup (password hashes, emails, payment references) in the same event. No second copy exists anywhere. For a product handling real payment data, this is a single point of total, unrecoverable failure.
- **Recommended fix:** Encrypt backup archives at rest (e.g. `age`/`gpg` before upload) and enable the already-stubbed off-site sync (`rclone` to an independent provider/region) as a required, not optional, step in the deploy runbook.

---

# 3. High Priority Issues

### H-1. UTF-8 boundary panic risk on the desktop parsing hot path
- **Evidence:** `src/parser/excel_parser.rs:329,370,377,411,456`, `src/ai_classifier.rs:201,238,275` — `&s[..s.len().min(N)]` byte-index slicing with no `is_char_boundary` guard.
- **Risk:** Any multi-byte character (₹, accented names, OCR noise) landing on the cut boundary panics the process on real production data — not a hypothetical edge case for an Indian bank-statement tool.
- **Recommended fix:** Replace with a char-boundary-safe truncation helper (e.g. round down to the nearest valid boundary, or use `.chars().take(N)`).

### H-2. Mutex poisoning with no recovery path
- **Evidence:** `src/main.rs` — 148 `.lock().unwrap()` sites on shared app state; zero `catch_unwind` anywhere in the repository.
- **Risk:** Any panic while a lock is held (H-1 is the most likely trigger) poisons the mutex and bricks the entire desktop app until a manual restart, mid-session, with no recovery.
- **Recommended fix:** Wrap the panic-risk region in `catch_unwind`, or replace `.unwrap()` on lock acquisition with poison-tolerant recovery (`.unwrap_or_else(|e| e.into_inner())`) at minimum until H-1 is fully closed.

### H-3. Silent DB-write failure on import
- **Evidence:** `src/main.rs:974-977` — `db::save_import(...).ok()` discards failure with no log or user-facing toast, unlike the sibling `upsert_transactions` failure path a few lines later, which is logged/toasted.
- **Risk:** A user can lose an entire imported statement with zero indication anything went wrong.
- **Recommended fix:** Match the existing `upsert_transactions` error-handling pattern — log and surface a toast on failure.

### H-4. No CI/CD; payment-critical test suite never runs automatically
- **Evidence:** No `.github/` directory or other CI config anywhere in the repo. 19 of 37 server integration tests (`auth_flow`, `license_flow`, `payment_flow`, `reconciliation_flow`, `rate_limit_flow`, `least_privilege_role` — all Postgres-backed) are `#[ignore]`d, requiring a manually supplied `DATABASE_URL`.
- **Risk:** A regression in webhook atomicity, reconciliation, or auth can merge to `main` fully green — the tests that would actually catch it are opt-in only.
- **Recommended fix:** Stand up CI (GitHub Actions or equivalent) that provisions a throwaway Postgres and runs the full ignored suite on every PR/merge to `main`.

### H-5. Migration-ownership landmine
- **Evidence:** `migrations/0003_least_privilege_app_role.sql` grants the restricted `license_server_app` role no `ALTER`/ownership on the 7 pre-existing tables (only `ALTER DEFAULT PRIVILEGES` for *future* tables). `migrations/0004_add_payment_dispute_support.sql` does `ALTER TABLE payments ADD COLUMN` / `DROP`/`ADD CONSTRAINT`, which requires ownership.
- **Risk:** Not actively broken today — 0004 was applied while still on admin credentials, before the documented role switch. But `server/README.md` instructs running migrations "at every startup" under the restricted role going forward; the **next** migration that needs `ALTER` will fail with "must be owner of relation" at deploy time, with no warning today.
- **Recommended fix:** Either grant the app role `ALTER`/ownership on the existing tables now, or explicitly codify "run migrations as admin, then re-sync grants" as a mandatory deploy step before the next schema change ships.

### H-6. Subscription cancellation does not revoke the license
- **Evidence:** `payment_service.rs` `resolve_subscription_inactive` only produces `WebhookMutation::UpdateSubscriptionStatus`, touching `subscriptions` only; `license_service::validate`/`heartbeat` never references `subscription_repository` at all. The current test is literally named `subscription_cancelled_updates_status_without_touching_the_license` — it documents the gap, not a fix.
- **Risk:** A cancelled or payment-failed subscriber keeps a fully valid, working license for the remainder of the original term — a direct revenue-integrity gap.
- **Recommended fix:** This needs a product decision (grace-period-to-term-end is a legitimate choice), but it must be a deliberate one — either wire cancellation through to license status, or explicitly document "access continues until term end" as intended behavior.

### H-7. No captured-amount/currency verification against stored payment
- **Evidence:** `RazorpayPayment`/`PaymentListItem` carry only `id`/`order_id`/`status` — no amount/currency anywhere in the webhook extraction or reconciliation path; `resolve_activation` never compares against `payments.amount_minor`.
- **Risk:** A partial capture (or any future amount-tampering vector) still grants full entitlement.
- **Recommended fix:** Extract `amount`/`currency` from the webhook payload and reject/flag activation when it doesn't match the stored `payments.amount_minor`.

### H-8. Rate-limiting gaps (three, all self-documented in-code as known/unfixed)
- **Evidence:** `rate_limit.rs:108-118` — `/login` keys on peer IP, which behind the documented Caddy topology is always Caddy's own container IP (one global bucket for all users). `rate_limit.rs:145-178` — `device_rate_limit` keys on attacker-controlled `device_id` from the request body with no pruning (unbounded memory growth). `rate_limit.rs:151` — `axum::body::to_bytes(body, usize::MAX)` with no `DefaultBodyLimit`/`RequestBodyLimitLayer` anywhere.
- **Risk:** Effective rate limiting collapses to a shared global bucket; memory grows unbounded over the life of the process; an oversized POST can be fully buffered into memory (DoS).
- **Recommended fix:** Trust `X-Forwarded-For` from Caddy specifically (not blindly from any client) for IP-keying; add TTL-based pruning or bound the keyed-limiter map size; add `DefaultBodyLimit` to the router.

### H-9. Raw DB error text leaked in API responses
- **Evidence:** `repository/error.rs:16-23` (`"database error: {e}"` wrapping raw `sqlx::Error`) surfaces verbatim into JSON 500 bodies via `routes/error.rs:102-135`; `routes/ready.rs:40-47` does the same on the unauthenticated `/readyz`.
- **Risk:** Leaks schema/constraint detail to any caller, including unauthenticated ones.
- **Recommended fix:** Log the detailed error server-side; return a generic message to the client.

### H-10. 6 of 11 real bank PDF fixtures parse to zero transactions
- **Evidence:** `tests/import_pipeline.rs:31-243` — BOB, ICICI, ICICI Wealth Mgmt, IDFC First, Union Bank, Cosmos Co-op fixtures fail, covered only by `#[ignore]`d tests (not run by default `cargo test`).
- **Risk:** Core product functionality — a customer using one of these banks gets silent zero-transaction imports on day one.
- **Recommended fix:** Fix the underlying parser bugs (Identity-H CID issue affects 4 of the 6) before advertising support for these banks; un-ignore the tests once fixed.

### H-11. SQLCipher `.bak` plaintext file never deleted
- **Evidence:** `src/db/encryption.rs:266-267` — explicitly retains the plaintext pre-migration backup; the test `migrates_existing_plaintext_db_and_preserves_all_data` asserts it remains plaintext-readable indefinitely.
- **Risk:** A full, permanent, unencrypted copy of all client/transaction data sits on disk beside the encrypted DB — defeats encryption at rest.
- **Recommended fix:** Securely delete (or prompt the user to delete) the `.bak` file once migration is verified successful.

### H-12. Backup-restore drill isn't wired into anything
- **Evidence:** `server/deploy/test-backup-restore.sh` is referenced only in `server/README.md`; no cron entry, Makefile, or CI invokes it.
- **Risk:** A later edit to `backup.sh`/`restore.sh`, or Postgres/psql/gzip version drift, silently breaks disaster recovery until an actual incident.
- **Recommended fix:** Add a scheduled (cron or CI) job that runs the drill and alerts on failure.

---

# 4. Medium Priority Issues

### M-1. License endpoints have no session/ownership check
- **Evidence:** `routes/license.rs:42-61` — `/validate-license`, `/heartbeat`, `/deactivate-license`, `/refresh-license` take only `license_id` (sequential, enumerable `BIGSERIAL`) + `device_id` from the body; `routes/auth.rs:114-118` documents this as deliberate.
- **Risk:** Weaker boundary than `/subscription`'s session-protected endpoint. Practical exploitability is low since `device_id` is a UUID v4 and also gates authorization, but it's an inconsistent security posture.
- **Recommended fix:** Either accept as a documented deliberate tradeoff, or add lightweight device-token binding at activation time.

### M-2. Non-unique correlation indexes
- **Evidence:** `payments.provider_ref` (migration `0002`) and the newer `payments.gateway_payment_id` (migration `0004`) both have non-unique indexes only; `find_by_provider_ref`/`find_by_gateway_payment_id` silently pick the most recent row on collision.
- **Risk:** A genuine id collision (unlikely but possible) resolves silently to the wrong payment.
- **Recommended fix:** Add `UNIQUE` constraints once confirmed no legitimate duplicate rows exist in production data.

### M-3. Hardcoded webhook event allowlist
- **Evidence:** `payment_service.rs` `process_webhook_event`'s match statement handles a fixed list of event types; any other real Razorpay event falls to the `other` arm, logged at `info` and dropped.
- **Risk:** A new or unanticipated Razorpay event type is silently ignored rather than flagged for review.
- **Recommended fix:** Log unrecognized events at `warn`, or alert on them, so gaps in coverage are visible rather than silent.

### M-4. Orphaned `pending_payment` subscription rows possible
- **Evidence:** `create_checkout_session` performs subscription-insert → Razorpay call → payment-insert as three unwrapped writes.
- **Risk:** A failure between steps leaves an orphaned row with no cleanup job.
- **Recommended fix:** Add a periodic cleanup job for stale `pending_payment` rows past a reasonable TTL.

### M-5. No server-side request/DB timeout layer
- **Evidence:** No `tower_http::timeout::TimeoutLayer` or `sqlx` statement-timeout configuration found in `lib.rs`/`main.rs` (only the Razorpay HTTP client has its own timeouts).
- **Risk:** A stuck downstream query has no server-enforced cutoff.
- **Recommended fix:** Add a request-level timeout middleware and/or a Postgres `statement_timeout`.

### M-6. Deployment health/observability gaps
- **Evidence:** `docker-compose.yml` — Caddy's `depends_on: [license-server]` has no `condition: service_healthy` (and `server/Dockerfile` defines no `HEALTHCHECK` for the license-server image to begin with); `Caddyfile` has no HSTS/X-Content-Type-Options/X-Frame-Options/CSP headers.
- **Risk:** A hung-but-listening server process is never auto-restarted; missing headers is a minor hardening gap on a payment-adjacent surface.
- **Recommended fix:** Add a `HEALTHCHECK` to the server Dockerfile, wire `condition: service_healthy` in compose, add baseline security headers to the Caddyfile.

### M-7. Undocumented reconciliation env vars
- **Evidence:** `RECONCILIATION_INTERVAL_SECS`/`BATCH_SIZE`/`MAX_AGE_HOURS` (defaults 900s/100/2h) are read and validated in `config.rs` but not listed in `server/.env.example`.
- **Risk:** An operator has no discoverable way to know these tunables exist without reading source.
- **Recommended fix:** Add commented entries to `.env.example`.

### M-8. `heartbeat()` implemented but never called from the desktop app
- **Evidence:** Present on the trait and in `HttpLicenseClient`, but zero call sites in `main.rs`. Periodic revalidation relies solely on `validate_license` via a 24h timer.
- **Risk:** Dead capability; if `heartbeat` was intended as a lighter-weight, more-frequent check, that benefit isn't realized.
- **Recommended fix:** Either wire it into a shorter-interval periodic check, or remove it to avoid confusion about which mechanism is authoritative.

### M-9. Compiled-in HMAC keys for local tamper detection and monthly-password auth
- **Evidence:** `storage.rs:36` (`INTEGRITY_KEY`) and `auth/monthly_password.rs:28-37` (`SK_FRAGMENTS`) are both compiled into the binary; both are self-documented in-code as anti-tamper/anti-piracy nudges, not attacker-resistant.
- **Risk:** A determined attacker with the binary can extract the key and forge valid local state. This is a known, accepted, documented tradeoff — flagged so it is never mistaken for a real security boundary in a future audit.
- **Recommended fix:** None required if the tradeoff remains accepted; otherwise move integrity verification server-side.

### M-10. `.env.example` still ships a restricted-role `DATABASE_URL`
- **Evidence:** `server/.env.example:23` points at `license_server_app`, while first-deploy requires superuser for the migration step; mitigated by explicit README instructions but the trap in the example file itself is unchanged.
- **Risk:** An operator copying the example verbatim without reading the README hits a chicken-and-egg failure on first deploy.
- **Recommended fix:** Comment the example value to point at the admin connection string for first-run, with a note to switch after migrations.

---

# 5. Low Priority Issues

- **Dead code** — `RECONCILIATION_INTERVAL_MINUTES` constant in `service::payment_service` is superseded by the Phase 4K.4 `interval_secs` config path and no longer used anywhere.
- **Repo hygiene** — stale `parity_audit_backup.patch`, `.patch_a_ai_classifier.diff`, accidentally-committed `ui/main_screen.slint.tmp` (contains only the text "read"), hardcoded developer path in `src/bin/pdf_diag.rs`.
- **`src/parser/ocr_extractor.rs` has zero unit tests** — the real Tesseract shell-out path is untested anywhere.
- **Build-config edge case** — `cargo build --no-default-features --features slint-ui` (without `ai`) fails: `HttpLicenseClient` is unconditionally re-exported from `license::mod.rs` but only defined under `#[cfg(feature = "ai")]`.
- **No root `README.md`** for the desktop crate — only `server/README.md` exists, no single onboarding doc for building/running the Slint app.

**Missing test coverage (severity-tagged, not independently blocking):**

| Gap | Severity |
|---|---|
| No test simulates Razorpay/DB/server unreachability mid-request (timeout paths exist in code, not exercised end-to-end) | Medium |
| No test for full process-restart recovery (state-survives-restart is only inferred from idempotency tests) | Medium |
| No test exercises the `main.rs` UI-glue wiring itself (login → `enforce` → `logged_in`/`license_blocked` transitions, the 24h timer) — all coverage is at the `license::` module level | Medium |
| No test proves a migration run under the restricted `license_server_app` role actually succeeds (ties to H-5) | Medium |
| Webhook out-of-order/delayed delivery untested (only duplicate-delivery idempotency is covered) | Low |
| Rate-limiter unbounded-growth/pruning behavior untested (ties to H-8) | Low |
| Request body-size limiting untested (ties to H-8, no limit exists to test) | Low |
| `heartbeat()` desktop call path untested (dead code, ties to M-8) | Low |

---

# 6. Recommended Future Improvements

Forward-looking, non-blocking enhancements — worth scheduling after Phase 4L.2, not required to reach a Go decision:

- **Reconciliation pagination + persisted watermark** — `list_payments_since` is single-page (batch-size cap) with no persisted "last successful run" watermark; add both if payment volume or outage duration grows beyond current fixed assumptions.
- **Digest-pin Docker base images** — `rust:1.90-slim-bookworm`/`debian:bookworm-slim` are tag-only, and `apt-get update` floats; pin by digest for reproducible builds.
- **Centralized log aggregation / OTel export** — tracing is currently stdout-only; add off-box log aggregation for real incident response at scale.
- **Migration rollback tooling** — migrations are forward-only today (acceptable, since none are destructive); consider `.down.sql` tooling as the schema grows.
- **Unique constraints on correlation columns** — once production data confirms no legitimate duplicates, tighten `provider_ref`/`gateway_payment_id` indexes to `UNIQUE` (closes M-2 permanently rather than just documenting the risk).
- **Device-token binding on license endpoints** — strengthen the M-1 boundary beyond `device_id`-only if a future audit decides the current posture is no longer acceptable.

---

# 7. Final Production Readiness Score

## **68 / 100**

*(up from 56/100 in the previous full audit — three of four prior Critical blockers independently re-verified as fixed: desktop enforcement, Payment Links correlation, refund/chargeback handling.)*

---

# 8. Go / No-Go Recommendation

## **No-Go** — not yet ready for production deployment.

The foundation is sound: license enforcement, payment correlation, and refund/chargeback handling — the three most dangerous gaps from the last audit — are now real, tested, and correctly wired end-to-end. The remaining gap is one Critical item (backup disaster-recovery posture) plus twelve High-severity items. None require an architecture change or a new feature; all are independently scoped, verifiable fixes.

---

# 9. Exact List of Fixes Required Before Phase 4L.2

1. Encrypt backup archives and enable off-site sync (C-1).
2. Fix the UTF-8 slicing panic risk and add panic recovery around the affected code path (H-1, H-2).
3. Fix the silently-swallowed import DB-write failure (H-3).
4. Stand up CI running the full Postgres-backed test suite on every merge (H-4).
5. Resolve the migration-ownership gap before the next schema change ships (H-5).
6. Make a deliberate product decision on subscription-cancellation → license behavior and implement it (H-6).
7. Add captured-amount/currency verification to webhook processing (H-7).
8. Close the three rate-limiting gaps: real client-IP trust behind Caddy, keyed-limiter pruning, request body-size limit (H-8).
9. Stop leaking raw DB error text in API responses (H-9).
10. Fix or explicitly scope out the 6 failing bank-PDF fixtures (H-10).
11. Stop retaining the plaintext SQLCipher `.bak` file (H-11).
12. Put the backup-restore drill on a real recurring schedule (H-12).

Once these twelve items are closed, no further architectural engineering work is anticipated before controlled production deployment — remaining work at that point would be environment-specific deployment and operational validation only. The Medium and Low items in this report, and the items in §6, are worth addressing but do not block Phase 4L.2 from starting.
