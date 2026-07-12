-- Non-payment license domain schema (LICENSE_DATABASE_SCHEMA.md §1,
-- PHASE4_DESIGN.md §7's `sessions` addition). `payments`/
-- `payment_webhook_events`/`login_history`/`license_validation_logs` are
-- intentionally excluded — out of scope until the payment/webhook/audit-
-- logging phases (see repository/mod.rs's doc comment).

-- One row per customer organization/individual.
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    full_name     TEXT,
    company_name  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A purchasable plan a user is subscribed to. History is kept via status
-- transitions on new rows, never by mutating a past row.
CREATE TABLE subscriptions (
    id                  BIGSERIAL PRIMARY KEY,
    user_id             BIGINT NOT NULL REFERENCES users(id),
    plan_type           TEXT NOT NULL CHECK (plan_type IN ('trial','monthly','yearly','lifetime')),
    status              TEXT NOT NULL CHECK (status IN ('active','expired','cancelled','suspended','pending_payment')),
    started_at          TIMESTAMPTZ NOT NULL,
    current_period_end  TIMESTAMPTZ,
    auto_renew          BOOLEAN NOT NULL DEFAULT true,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_subscriptions_user ON subscriptions(user_id);
CREATE INDEX idx_subscriptions_status ON subscriptions(status);

-- The actual activatable credential — kept separate from subscriptions so
-- "this exact key was activated on these devices" is independently
-- auditable from "this customer's billing status."
CREATE TABLE licenses (
    id                 BIGSERIAL PRIMARY KEY,
    subscription_id    BIGINT NOT NULL REFERENCES subscriptions(id),
    license_key        TEXT NOT NULL UNIQUE,
    status             TEXT NOT NULL CHECK (status IN ('active','revoked','expired','suspended')),
    expires_at         TIMESTAMPTZ,
    max_devices        INT NOT NULL DEFAULT 1,
    grace_period_days  INT NOT NULL DEFAULT 7,
    issued_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at         TIMESTAMPTZ,
    revoked_reason     TEXT
);
CREATE INDEX idx_licenses_subscription ON licenses(subscription_id);
CREATE UNIQUE INDEX idx_licenses_key ON licenses(license_key);

-- One row per (license, physical machine) activation.
CREATE TABLE devices (
    id                   BIGSERIAL PRIMARY KEY,
    license_id           BIGINT NOT NULL REFERENCES licenses(id),
    device_id            UUID NOT NULL,
    machine_fingerprint  TEXT NOT NULL,
    device_label         TEXT,
    first_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    deactivated_at       TIMESTAMPTZ,
    UNIQUE (license_id, device_id)
);
CREATE INDEX idx_devices_license ON devices(license_id);
CREATE INDEX idx_devices_fingerprint ON devices(machine_fingerprint);

-- Server-account bearer-token sessions (PHASE4_DESIGN.md §7 — added beyond
-- LICENSE_DATABASE_SCHEMA.md §1, which predates payment/auth needing real,
-- revocable session storage).
CREATE TABLE sessions (
    id          BIGSERIAL PRIMARY KEY,
    user_id     BIGINT NOT NULL REFERENCES users(id),
    token_hash  TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX idx_sessions_user ON sessions(user_id);
