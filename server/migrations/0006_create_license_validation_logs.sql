-- Audit logging, part 2 of 2 (LICENSE_DATABASE_SCHEMA.md §1, lines 106-118 —
-- specified since Phase 3A, never migrated until now). Every
-- validate-license/heartbeat/activate-license call, kept for support/
-- dispute resolution and anomaly detection.
--
-- `device_id` is a plain UUID, not a foreign key — the caller-supplied
-- device_id on a request that never resolved to a real `devices` row
-- (an unrecognized device attempting `/validate-license`, say) is still
-- worth recording, so this deliberately doesn't reference `devices(id)`.
--
-- `result` only covers the five outcomes this table's schema has always
-- anticipated. Newer `LicenseOperationError` variants that don't fit this
-- taxonomy (`LicenseNotFound` — no license row to attribute the row to;
-- `DeviceLimitReached` — a capacity error, not a license-state result) are
-- intentionally not written here by `service::license_service`'s call
-- sites; widening this CHECK constraint to cover them is separate,
-- explicitly-scoped follow-up work, not part of this migration.
CREATE TABLE license_validation_logs (
    id           BIGSERIAL PRIMARY KEY,
    license_id   BIGINT NOT NULL REFERENCES licenses(id),
    device_id    UUID NOT NULL,
    result       TEXT NOT NULL CHECK (result IN ('valid','expired','suspended','revoked','device_mismatch')),
    ip_address   INET,
    client_clock TIMESTAMPTZ,          -- reserved for future clock-rollback detection; not populated yet — see repository::audit's doc comment
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_validation_logs_license ON license_validation_logs(license_id, created_at DESC);
