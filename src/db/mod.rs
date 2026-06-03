// db/mod.rs — SQLite persistence layer.
// Phase 2: schema initialisation only.
// CRUD helpers for clients, transactions, ledgers, and audit log added in Phase 4.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// Open (or create) the SQLite database at `path` and run the schema migration.
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let conn = Connection::open(&path)
        .with_context(|| format!("Cannot open database at {:?}", path.as_ref()))?;

    // Enable WAL mode for better concurrent read performance and crash safety.
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("Failed to set WAL journal mode")?;
    conn.pragma_update(None, "foreign_keys", true)
        .context("Failed to enable foreign keys")?;

    init_schema(&conn).context("Schema initialisation failed")?;

    log::info!("Database opened at {:?}", path.as_ref());
    Ok(conn)
}

// ── Schema ────────────────────────────────────────────────────────────────────

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("execute_batch failed on schema SQL")?;
    Ok(())
}

const SCHEMA_SQL: &str = "
-- ── Clients ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS clients (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL UNIQUE,
    gstin       TEXT,
    address     TEXT,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- ── Transactions ────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS transactions (
    id              TEXT    PRIMARY KEY,          -- parser-assigned e.g. t_42_1717000000000
    client_id       INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    import_id       INTEGER REFERENCES import_history(id),

    -- Source bank
    bank_name       TEXT,
    account_no      TEXT,

    -- Date (stored as ISO text DD/MM/YYYY + unix-ms timestamp for fast sorting)
    date            TEXT,
    date_ts         INTEGER,

    -- Transaction data
    narration       TEXT,
    reference       TEXT,
    debit           REAL,
    credit          REAL,
    balance         REAL,
    prev_balance    REAL,

    -- Classification
    vendor          TEXT,
    account_head    TEXT,
    txn_type        TEXT,
    status          TEXT    NOT NULL DEFAULT 'unreviewed',
    confidence      REAL             DEFAULT 0,
    classified_by   TEXT,            -- 'auto' | 'ai' | 'manual'
    tags            TEXT,            -- JSON array  e.g. '[\"GST\",\"high\"]'
    balance_ok      INTEGER          DEFAULT 1,    -- 0 = mismatch flag
    is_opening_bal  INTEGER          DEFAULT 0,

    created_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_txn_client  ON transactions(client_id);
CREATE INDEX IF NOT EXISTS idx_txn_date_ts ON transactions(date_ts);
CREATE INDEX IF NOT EXISTS idx_txn_status  ON transactions(status);
CREATE INDEX IF NOT EXISTS idx_txn_vendor  ON transactions(vendor);

-- ── Ledgers ─────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS ledgers (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    group_name  TEXT,
    nature      TEXT,              -- 'Assets' | 'Liabilities' | 'Income' | 'Expenses'
    UNIQUE(client_id, name)
);

-- ── Classification rules ─────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS classification_rules (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    pattern     TEXT    NOT NULL,  -- keyword / regex pattern
    vendor      TEXT,
    account_head TEXT,
    txn_type    TEXT,
    priority    INTEGER DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- ── Import history ───────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS import_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    file_name   TEXT    NOT NULL,
    bank_name   TEXT,
    account_no  TEXT,
    txn_count   INTEGER DEFAULT 0,
    file_hash   TEXT,              -- SHA-256 of file bytes (dedup guard)
    imported_at TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- ── App settings (key-value) ──────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
";

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_db_initialises() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).expect("schema init failed on in-memory DB");

        // Verify tables exist
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='transactions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "transactions table must exist");
    }
}
