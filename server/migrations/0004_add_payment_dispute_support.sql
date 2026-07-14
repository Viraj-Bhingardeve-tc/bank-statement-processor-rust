-- Phase 4K.2 — refund/chargeback support. Purely additive: no existing
-- column, row, or constraint value is removed or renamed.
--
-- Razorpay's `refund.*`/`payment.dispute.*` webhooks only ever reference
-- the underlying payment's own gateway id (`payload.payment.entity.id`) —
-- never the Payment Link id or Subscription id already stored in
-- `payments.provider_ref` (the checkout-time reference). Without a second,
-- independent correlation key, a real refund/dispute webhook could never
-- be matched back to a `payments` row for any plan type. This column
-- closes that gap: it's populated once known, at activation time
-- (`payment.captured`/`payment_link.paid`/`subscription.charged` all
-- carry a `payment.entity.id`), and is the only field refund/dispute
-- correlation reads.
ALTER TABLE payments ADD COLUMN gateway_payment_id TEXT;
CREATE INDEX idx_payments_gateway_payment_id ON payments(gateway_payment_id);

-- Widen the `payments.status` CHECK constraint (0002) to allow
-- 'disputed' — a payment under an open Razorpay dispute, distinct from
-- 'succeeded' (no dispute, or one already resolved for the merchant) and
-- 'refunded' (money definitively returned). The constraint's
-- system-generated name is looked up dynamically rather than hardcoded,
-- so this doesn't depend on guessing Postgres's exact naming convention
-- for the original inline CHECK.
DO $$
DECLARE
    existing_constraint text;
BEGIN
    SELECT con.conname INTO existing_constraint
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    WHERE rel.relname = 'payments'
      AND con.contype = 'c'
      AND pg_get_constraintdef(con.oid) LIKE '%status%'
    LIMIT 1;

    IF existing_constraint IS NOT NULL THEN
        EXECUTE format('ALTER TABLE payments DROP CONSTRAINT %I', existing_constraint);
    END IF;
END $$;

ALTER TABLE payments ADD CONSTRAINT payments_status_check
    CHECK (status IN ('pending', 'succeeded', 'failed', 'refunded', 'disputed'));
