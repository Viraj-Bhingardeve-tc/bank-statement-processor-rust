-- Payment domain schema (LICENSE_DATABASE_SCHEMA.md §1's `payments` table,
-- PHASE4_DESIGN.md §7's `payment_webhook_events` addition). Deliberately
-- excluded from migration 0001 (Phase 4C.1/4D) — payment was out of scope
-- until this phase.

-- Payment ledger. One row per checkout attempt (Payment Links for
-- `lifetime`, the initial Subscription charge for `monthly`/`yearly`) —
-- see `repository::payment`'s doc comment for the recurring-charge
-- simplification this phase makes (renewals extend the original row
-- rather than inserting a new one per billing cycle).
CREATE TABLE payments (
    id              BIGSERIAL PRIMARY KEY,
    subscription_id BIGINT NOT NULL REFERENCES subscriptions(id),
    amount_minor    BIGINT NOT NULL,         -- smallest currency unit (paise), never a float
    currency        TEXT NOT NULL DEFAULT 'INR',
    provider        TEXT NOT NULL,           -- 'razorpay', 'manual', ...
    provider_ref    TEXT,                    -- gateway's own payment/order/subscription id
    status          TEXT NOT NULL CHECK (status IN ('pending','succeeded','failed','refunded')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_payments_subscription ON payments(subscription_id);
CREATE INDEX idx_payments_provider_ref ON payments(provider_ref);

-- Idempotency ledger for inbound provider webhooks (PHASE4_DESIGN.md §4
-- step 2 / §7) — `(provider, event_id)` is checked before any other write;
-- if already present, the webhook handler returns 200 and does nothing
-- further.
CREATE TABLE payment_webhook_events (
    id              BIGSERIAL PRIMARY KEY,
    provider        TEXT NOT NULL,          -- 'razorpay'
    event_id        TEXT NOT NULL,          -- provider's own idempotency key
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    processed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, event_id)
);
