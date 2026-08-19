-- End-to-end payment testing pass (Phase 4N) — the exact same class of bug
-- Finding H2 / migration 0008 fixed for `payments.provider_ref`, found
-- during a review of the refund/dispute flow to still be present for
-- `gateway_payment_id` (migration 0004: only ever a non-unique index,
-- `CREATE INDEX idx_payments_gateway_payment_id ON payments(gateway_payment_id)`).
-- `repository::payment::PgPaymentRepository::find_by_gateway_payment_id`
-- used to mask a collision by silently picking the most recently created
-- matching row (`ORDER BY created_at DESC LIMIT 1`) instead of erroring —
-- and since `gateway_payment_id` is the *only* field `refund.*`/
-- `payment.dispute.*` webhooks correlate against
-- (`domain::Payment::gateway_payment_id`'s doc comment), a collision here
-- could refund or suspend the wrong customer's license, exactly as
-- serious as the original H2 finding. This migration makes that
-- structurally impossible going forward.
--
-- Guard: same reasoning as migration 0008 — a bare "could not create
-- unique index" error doesn't tell an operator which rows collide or what
-- to do about it. This raises a clearer, actionable error first, and
-- nothing below it runs if it fires, so this migration is safe to re-run
-- once the colliding rows are resolved. Deciding *which* of two colliding
-- rows should keep the reference is a business/data-quality judgment call
-- this migration deliberately leaves to an operator, not something a
-- schema migration should silently guess at.
DO $$
DECLARE
    duplicate_count integer;
BEGIN
    SELECT COUNT(*) INTO duplicate_count
    FROM (
        SELECT gateway_payment_id
        FROM payments
        WHERE gateway_payment_id IS NOT NULL
        GROUP BY gateway_payment_id
        HAVING COUNT(*) > 1
    ) AS duplicates;

    IF duplicate_count > 0 THEN
        RAISE EXCEPTION 'migration 0009 aborted: % distinct gateway_payment_id value(s) are shared by more than one payments row — resolve which row should keep the reference before re-running this migration', duplicate_count;
    END IF;
END $$;

-- Partial (`WHERE gateway_payment_id IS NOT NULL`) for the same reasons
-- migration 0008 uses a partial index on `provider_ref`: the column is
-- nullable (unset until an activating webhook supplies one) and this is
-- the self-documenting statement of intent — "this uniqueness rule is
-- about real gateway payment ids, not the absence of one."
CREATE UNIQUE INDEX idx_payments_gateway_payment_id_unique
    ON payments(gateway_payment_id)
    WHERE gateway_payment_id IS NOT NULL;

DROP INDEX idx_payments_gateway_payment_id;
