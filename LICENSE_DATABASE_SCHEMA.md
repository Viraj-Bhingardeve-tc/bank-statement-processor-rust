# License Database Schema — Phase 3A

Two separate schemas, deliberately not conflated:

- **§1 — Server-side schema.** Design only. No server exists yet. This is what a future backend (whatever stack it's built in) should implement to match `API_SPECIFICATION.md`.
- **§2 — Desktop-side local cache schema.** Implemented now, in this phase, as SQLite migration 6 in `src/db/mod.rs`, on the app's existing per-installation encrypted database.

The desktop **never** stores `users` or `payments` tables — those are server-owned, multi-tenant data with no business being replicated onto every customer's laptop. The desktop stores only the minimum needed to (a) identify itself to the server and (b) work offline within the grace period.

---

## 1. Server-side schema (design only)

All tables `snake_case`, `id` primary keys, foreign keys `ON DELETE RESTRICT` unless noted (license data should never silently cascade-delete).

```sql
-- One row per customer organization/individual.
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,           -- server-side auth, unrelated to the desktop's monthly_password gate
    full_name     TEXT,
    company_name  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A purchasable plan a user is subscribed to. One user can have multiple
-- subscriptions over time (renewals, upgrades) — history is kept, not
-- overwritten, via status transitions rather than row mutation of past rows.
CREATE TABLE subscriptions (
    id                BIGSERIAL PRIMARY KEY,
    user_id           BIGINT NOT NULL REFERENCES users(id),
    plan_type         TEXT NOT NULL CHECK (plan_type IN ('trial','monthly','yearly','lifetime')),
    status            TEXT NOT NULL CHECK (status IN ('active','expired','cancelled','suspended','pending_payment')),
    started_at        TIMESTAMPTZ NOT NULL,
    current_period_end TIMESTAMPTZ,         -- NULL for lifetime
    auto_renew        BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_subscriptions_user ON subscriptions(user_id);
CREATE INDEX idx_subscriptions_status ON subscriptions(status);

-- The actual activatable credential. One subscription can (in principle)
-- issue more than one license over its life (e.g. reissued after a support
-- ticket) — kept separate from subscriptions so "this exact key was
-- activated on these devices" is independently auditable from "this
-- customer's billing status."
CREATE TABLE licenses (
    id              BIGSERIAL PRIMARY KEY,
    subscription_id BIGINT NOT NULL REFERENCES subscriptions(id),
    license_key     TEXT NOT NULL UNIQUE,   -- customer-facing activation code
    status          TEXT NOT NULL CHECK (status IN ('active','revoked','expired','suspended')),
    expires_at      TIMESTAMPTZ,            -- NULL for lifetime
    max_devices     INT NOT NULL DEFAULT 1,
    grace_period_days INT NOT NULL DEFAULT 7,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at      TIMESTAMPTZ,
    revoked_reason  TEXT
);
CREATE INDEX idx_licenses_subscription ON licenses(subscription_id);
CREATE UNIQUE INDEX idx_licenses_key ON licenses(license_key);

-- One row per (license, physical machine) activation.
CREATE TABLE devices (
    id                BIGSERIAL PRIMARY KEY,
    license_id        BIGINT NOT NULL REFERENCES licenses(id),
    device_id         UUID NOT NULL,         -- client-generated, see LICENSE_SYSTEM_DESIGN.md §5
    machine_fingerprint TEXT NOT NULL,
    device_label      TEXT,                  -- e.g. "DESKTOP-AB12CD" — for the admin dashboard's device list
    first_seen_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deactivated_at    TIMESTAMPTZ,
    UNIQUE (license_id, device_id)
);
CREATE INDEX idx_devices_license ON devices(license_id);
CREATE INDEX idx_devices_fingerprint ON devices(machine_fingerprint);

-- Payment ledger. Deliberately schema-ready now even though Phase 3A does
-- not integrate a gateway — a future Razorpay integration only needs to
-- start writing rows here and updating subscriptions.status, not add new
-- tables.
CREATE TABLE payments (
    id              BIGSERIAL PRIMARY KEY,
    subscription_id BIGINT NOT NULL REFERENCES subscriptions(id),
    amount_minor    BIGINT NOT NULL,         -- smallest currency unit (paise), never a float
    currency        TEXT NOT NULL DEFAULT 'INR',
    provider        TEXT NOT NULL,           -- 'razorpay', 'manual', ...
    provider_ref    TEXT,                    -- gateway's own payment/order id
    status          TEXT NOT NULL CHECK (status IN ('pending','succeeded','failed','refunded')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_payments_subscription ON payments(subscription_id);

CREATE TABLE login_history (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id),
    device_id  UUID,
    ip_address INET,
    success    BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_login_history_user ON login_history(user_id, created_at DESC);

-- Every validate-license/heartbeat call, kept for support/dispute
-- resolution and anomaly detection (e.g. one device_id validating from many
-- distinct IPs in a short window).
CREATE TABLE license_validation_logs (
    id           BIGSERIAL PRIMARY KEY,
    license_id   BIGINT NOT NULL REFERENCES licenses(id),
    device_id    UUID NOT NULL,
    result       TEXT NOT NULL CHECK (result IN ('valid','expired','suspended','revoked','device_mismatch')),
    ip_address   INET,
    client_clock TIMESTAMPTZ,          -- what the client claimed "now" was — for clock-rollback detection, §LICENSE_SECURITY_REVIEW.md
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_validation_logs_license ON license_validation_logs(license_id, created_at DESC);
```

---

## 2. Desktop-side local cache schema (implemented — migration 6)

Added to the existing `transactions.db`-style encrypted SQLite database, via the established `db::MIGRATIONS` framework (see `db/mod.rs`). Three tables, intentionally small:

```sql
-- Single-row cache of the currently-activated license (or empty = not
-- activated). Not "one row per license ever seen" — this app only ever
-- cares about the current one; history lives server-side in
-- license_validation_logs, not duplicated locally.
CREATE TABLE IF NOT EXISTS local_license (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),  -- enforces single-row
    customer_id         TEXT,
    license_id          TEXT,
    license_key         TEXT,
    subscription_type   TEXT,     -- 'trial' | 'monthly' | 'yearly' | 'lifetime' | NULL
    status              TEXT NOT NULL DEFAULT 'not_activated',
    expires_at          TEXT,     -- ISO-8601 UTC
    last_validated_at   TEXT,     -- ISO-8601 UTC
    grace_period_days   INTEGER NOT NULL DEFAULT 7,
    highest_seen_clock  TEXT,     -- clock-rollback watermark, see LICENSE_SECURITY_REVIEW.md §1
    updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

-- This installation's identity. Also effectively single-row today (one
-- desktop app instance = one device), kept as its own table rather than
-- folded into local_license so the device identity survives a
-- deactivate/reactivate cycle that clears local_license's subscription
-- fields.
CREATE TABLE IF NOT EXISTS device_info (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    device_id           TEXT NOT NULL,
    machine_fingerprint TEXT NOT NULL,
    fingerprint_inputs  TEXT,     -- JSON: which raw signals were hashed, for support diagnostics
    created_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Append-only local audit trail of every validation attempt (online or
-- offline-grace), independent of whether the server was reachable —
-- mirrors license_validation_logs' purpose but client-side, and useful even
-- before a server exists (support can ask a customer to export this table).
CREATE TABLE IF NOT EXISTS license_validation_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    checked_at TEXT NOT NULL DEFAULT (datetime('now')),
    result     TEXT NOT NULL,     -- LicenseStatus variant name
    online     INTEGER NOT NULL,  -- 1 = reached the server, 0 = offline/grace path
    detail     TEXT               -- free-text, e.g. "grace period: 3 days remaining"
);
CREATE INDEX IF NOT EXISTS idx_license_validation_log_time ON license_validation_log(checked_at DESC);
```

### Why `CHECK (id = 1)` instead of a schemaless key-value row in the existing `settings` table

This app already has a `settings` key-value table (`db::get_setting`/`set_setting`). License state was *not* piggybacked onto it, for two reasons: (1) license data needs real columns with real types for the expiry-math and clock-rollback logic to be checked at the SQL level where useful (`CHECK` constraints, indexed queries on `checked_at`), which a stringly-typed key-value store can't express; (2) keeping it as dedicated tables makes the migration itself the single source of truth for the schema shape, reviewable in one place (`db/mod.rs`'s `MIGRATIONS` list) exactly like every other structural change in this codebase, rather than an implicit convention about which `settings` keys "belong" to licensing.

### Migration placement

`(6, "CREATE TABLE IF NOT EXISTS local_license (...); CREATE TABLE IF NOT EXISTS device_info (...); CREATE TABLE IF NOT EXISTS license_validation_log (...); CREATE INDEX ...;")` — appended to `MIGRATIONS`, following the exact pattern of migrations 1-5 (a single `execute_batch`, `PRAGMA user_version` advanced automatically by the existing `apply_migrations` function). No changes to `apply_migrations` itself, no changes to `SCHEMA_SQL` (matches this codebase's established convention: schema evolution happens entirely via `MIGRATIONS`, uniformly for fresh installs and upgrades — see `CROSS_CLIENT_TRANSACTION_ID_FIX_REPORT.md` for where this convention was last confirmed and relied on).
