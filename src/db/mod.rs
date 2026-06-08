// db/mod.rs — SQLite persistence layer with full CRUD.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

// ── Public data types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Client {
    pub id:           i64,
    pub name:         String,
    pub tally_ledger: String,
}

#[derive(Debug, Clone)]
pub struct ImportRecord {
    pub id:          i64,
    pub client_id:   i64,
    pub file_name:   String,
    pub bank_name:   String,
    pub account_no:  String,
    pub txn_count:   i64,
    pub imported_at: String,
}

#[derive(Debug, Clone)]
pub struct ClassificationRule {
    pub id:          i64,
    pub client_id:   i64,
    pub pattern:     String,
    pub vendor:      String,
    pub account_head: String,
    pub txn_type:    String,
}

// ── Client CRUD ───────────────────────────────────────────────────────────────

pub fn add_client(conn: &Connection, name: &str, tally_ledger: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO clients (name, gstin) VALUES (?1, ?2)",
        rusqlite::params![name, tally_ledger],
    ).context("add_client insert")?;
    Ok(conn.last_insert_rowid())
}

pub fn get_clients(conn: &Connection) -> Result<Vec<Client>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, gstin FROM clients ORDER BY name"
    ).context("get_clients prepare")?;
    let rows = stmt.query_map([], |r| {
        Ok(Client {
            id:           r.get(0)?,
            name:         r.get(1)?,
            tally_ledger: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        })
    }).context("get_clients query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("get_clients collect")
}

pub fn get_client(conn: &Connection, id: i64) -> Result<Option<Client>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, gstin FROM clients WHERE id = ?1"
    ).context("get_client prepare")?;
    let mut rows = stmt.query_map(rusqlite::params![id], |r| {
        Ok(Client {
            id:           r.get(0)?,
            name:         r.get(1)?,
            tally_ledger: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        })
    }).context("get_client query")?;
    Ok(rows.next().transpose().context("get_client row")?)
}

// ── Import history CRUD ───────────────────────────────────────────────────────

pub fn save_import(
    conn: &Connection, client_id: i64, file_name: &str,
    bank_name: &str, account_no: &str, txn_count: usize,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO import_history (client_id, file_name, bank_name, account_no, txn_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![client_id, file_name, bank_name, account_no, txn_count as i64],
    ).context("save_import insert")?;
    Ok(conn.last_insert_rowid())
}

pub fn get_imports(conn: &Connection, client_id: i64) -> Result<Vec<ImportRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, client_id, file_name, bank_name, account_no, txn_count, imported_at
         FROM import_history WHERE client_id = ?1
         ORDER BY imported_at DESC LIMIT 20"
    ).context("get_imports prepare")?;
    let rows = stmt.query_map(rusqlite::params![client_id], |r| {
        Ok(ImportRecord {
            id:          r.get(0)?,
            client_id:   r.get(1)?,
            file_name:   r.get(2)?,
            bank_name:   r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            account_no:  r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            txn_count:   r.get(5)?,
            imported_at: r.get(6)?,
        })
    }).context("get_imports query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("get_imports collect")
}

pub fn delete_import(conn: &Connection, import_id: i64) -> Result<()> {
    conn.execute("DELETE FROM import_history WHERE id = ?1", rusqlite::params![import_id])
        .context("delete_import")?;
    Ok(())
}

// ── Dedup hash CRUD ───────────────────────────────────────────────────────────

pub fn add_rule(
    conn: &Connection, client_id: i64,
    pattern: &str, vendor: &str, account_head: &str, txn_type: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO classification_rules (client_id, pattern, vendor, account_head, txn_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![client_id, pattern, vendor, account_head, txn_type],
    ).context("add_rule")?;
    Ok(())
}

pub fn get_rules(conn: &Connection, client_id: i64) -> Result<Vec<ClassificationRule>> {
    let mut stmt = conn.prepare(
        "SELECT id, client_id, pattern, vendor, account_head, txn_type
         FROM classification_rules WHERE client_id = ?1 OR client_id = 0
         ORDER BY priority DESC, id"
    ).context("get_rules prepare")?;
    let rows = stmt.query_map(rusqlite::params![client_id], |r| {
        Ok(ClassificationRule {
            id:           r.get(0)?,
            client_id:    r.get(1)?,
            pattern:      r.get(2)?,
            vendor:       r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            account_head: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            txn_type:     r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        })
    }).context("get_rules query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("get_rules collect")
}

pub fn delete_rule(conn: &Connection, rule_id: i64) -> Result<()> {
    conn.execute("DELETE FROM classification_rules WHERE id = ?1", rusqlite::params![rule_id])
        .context("delete_rule")?;
    Ok(())
}

// ── Settings CRUD ─────────────────────────────────────────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")
        .context("get_setting prepare")?;
    let mut rows = stmt.query_map(rusqlite::params![key], |r| r.get(0))
        .context("get_setting query")?;
    Ok(rows.next().transpose().context("get_setting row")?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        rusqlite::params![key, value],
    ).context("set_setting")?;
    Ok(())
}

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
