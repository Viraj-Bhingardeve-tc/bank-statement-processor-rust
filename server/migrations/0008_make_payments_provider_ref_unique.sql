-- Production Hardening, Finding H2 — `payments.provider_ref` has only
-- ever had a non-unique index (migration 0002:
-- `CREATE INDEX idx_payments_provider_ref ON payments(provider_ref)`).
-- `repository::payment::PgPaymentRepository::find_by_provider_ref` used to
-- mask a collision by silently picking the most recently created matching
-- row instead of erroring — the wrong payment could be mutated by a
-- webhook if two rows ever shared a `provider_ref`. This migration makes
-- that structurally impossible going forward.
--
-- Guard: if any non-null `provider_ref` value already appears on more
-- than one row, creating the `UNIQUE` index below would abort with a bare
-- Postgres "could not create unique index" constraint-violation error —
-- accurate, but it doesn't tell an operator *which* rows collide or what
-- to do about it. This raises a clearer, actionable error first, and
-- nothing below it runs if it fires, so this migration is safe to
-- re-run once the colliding rows are resolved. Deciding *which* of two
-- colliding rows should keep the reference is a business/data-quality
-- judgment call this migration deliberately leaves to an operator, not
-- something a schema migration should silently guess at.
DO $$
DECLARE
    duplicate_count integer;
BEGIN
    SELECT COUNT(*) INTO duplicate_count
    FROM (
        SELECT provider_ref
        FROM payments
        WHERE provider_ref IS NOT NULL
        GROUP BY provider_ref
        HAVING COUNT(*) > 1
    ) AS duplicates;

    IF duplicate_count > 0 THEN
        RAISE EXCEPTION 'migration 0008 aborted: % distinct provider_ref value(s) are shared by more than one payments row — resolve which row should keep the reference before re-running this migration', duplicate_count;
    END IF;
END $$;

-- Partial (`WHERE provider_ref IS NOT NULL`) since `provider_ref` is
-- nullable (a payment row can exist before checkout ever gets a gateway
-- reference back — `repository::payment`'s own doc comment) and multiple
-- `NULL`s must never collide with each other the way `UNIQUE` alone would
-- otherwise (mis)treat them under a plain non-partial index... actually
-- Postgres already treats NULL as distinct-from-NULL under a normal
-- UNIQUE index, but the partial form is kept anyway: it's the more
-- self-documenting statement of intent ("this uniqueness rule is about
-- real gateway references, not the absence of one"), and keeps the index
-- smaller by excluding every not-yet-referenced row.
CREATE UNIQUE INDEX idx_payments_provider_ref_unique
    ON payments(provider_ref)
    WHERE provider_ref IS NOT NULL;

DROP INDEX idx_payments_provider_ref;
