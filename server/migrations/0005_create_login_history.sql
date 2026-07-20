-- Audit logging, part 1 of 2 (LICENSE_DATABASE_SCHEMA.md §1, lines 96-104 —
-- specified since Phase 3A, never migrated until now). Append-only history
-- of `/login` attempts, kept for support/dispute resolution.
--
-- `user_id` is `NOT NULL REFERENCES users(id)`, so an attempt against an
-- email with no matching `users` row has nothing to reference and is
-- intentionally never inserted here (see `service::auth_service::login`'s
-- audit-write call site) — this table only ever records attempts against a
-- real, known account (successful or wrong-password).
CREATE TABLE login_history (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id),
    device_id  UUID,
    ip_address INET,
    success    BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_login_history_user ON login_history(user_id, created_at DESC);
