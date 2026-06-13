// db/mod.rs — SQLite persistence layer with full CRUD.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use crate::parser::{Transaction, TransactionStatus, VoucherType};

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

pub fn get_client_by_name(conn: &Connection, name: &str) -> Result<Option<Client>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, gstin FROM clients WHERE name = ?1"
    ).context("get_client_by_name prepare")?;
    let mut rows = stmt.query_map(rusqlite::params![name], |r| {
        Ok(Client {
            id:           r.get(0)?,
            name:         r.get(1)?,
            tally_ledger: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        })
    }).context("get_client_by_name query")?;
    Ok(rows.next().transpose().context("get_client_by_name row")?)
}

pub fn update_client(conn: &Connection, id: i64, name: &str, tally_ledger: &str) -> Result<()> {
    conn.execute(
        "UPDATE clients SET name = ?1, gstin = ?2, updated_at = datetime('now') WHERE id = ?3",
        rusqlite::params![name, tally_ledger, id],
    ).context("update_client")?;
    Ok(())
}

pub fn delete_client(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM clients WHERE id = ?1", rusqlite::params![id])
        .context("delete_client")?;
    Ok(())
}

// ── Transaction CRUD ──────────────────────────────────────────────────────────

fn voucher_from_str(s: &str) -> VoucherType {
    match s {
        "Payment"  => VoucherType::Payment,
        "Receipt"  => VoucherType::Receipt,
        "Contra"   => VoucherType::Contra,
        "Journal"  => VoucherType::Journal,
        "Sales"    => VoucherType::Sales,
        "Purchase" => VoucherType::Purchase,
        _          => VoucherType::Unknown,
    }
}

fn status_from_str(s: &str) -> TransactionStatus {
    match s {
        "classified"   => TransactionStatus::Classified,
        "manual"       => TransactionStatus::Manual,
        "suspense"     => TransactionStatus::Suspense,
        "needs_review" => TransactionStatus::NeedsReview,
        _              => TransactionStatus::Unreviewed,
    }
}

pub fn upsert_transactions(
    conn: &Connection,
    client_id: i64,
    import_id: Option<i64>,
    txns: &[Transaction],
) -> Result<usize> {
    let mut count = 0usize;
    for t in txns {
        let tags_json = serde_json::to_string(&t.tags).unwrap_or_else(|_| "[]".to_string());
        let eff_import_id = import_id.or(t.import_id);
        conn.execute(
            "INSERT OR REPLACE INTO transactions (
                id, client_id, import_id, bank_name, account_no,
                date, date_ts, narration, reference,
                debit, credit, balance, prev_balance,
                vendor, account_head, txn_type, status, confidence,
                classified_by, tags, balance_ok, is_opening_bal, dup_flag
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18,
                ?19, ?20, ?21, ?22, ?23
            )",
            rusqlite::params![
                t.id, client_id, eff_import_id, t.bank_name, t.account_no,
                t.date, t.date_ts, t.narration, t.reference,
                t.debit, t.credit, t.balance, t.prev_balance,
                t.vendor, t.account_head, t.txn_type.to_string(), t.status.to_string(), t.confidence,
                Option::<String>::None,
                tags_json,
                t.balance_ok.map(|b| if b { 1i64 } else { 0i64 }),
                if t.is_opening_balance { 1i64 } else { 0i64 },
                if t.dup_flag { 1i64 } else { 0i64 },
            ],
        ).context("upsert_transaction row")?;
        count += 1;
    }
    Ok(count)
}

pub fn get_transactions(conn: &Connection, client_id: i64) -> Result<Vec<Transaction>> {
    let mut stmt = conn.prepare(
        "SELECT id, import_id, bank_name, account_no, date, date_ts,
                narration, reference, debit, credit, balance, prev_balance,
                vendor, account_head, txn_type, status, confidence, tags,
                balance_ok, is_opening_bal, dup_flag
         FROM transactions WHERE client_id = ?1
         ORDER BY date_ts ASC, rowid ASC"
    ).context("get_transactions prepare")?;
    let rows = stmt.query_map(rusqlite::params![client_id], |r| {
        let tags_json: Option<String> = r.get(17)?;
        let tags: Vec<String> = tags_json
            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
            .unwrap_or_default();
        let balance_ok_raw: Option<i64> = r.get(18)?;
        let is_ob: i64 = r.get::<_, Option<i64>>(19)?.unwrap_or(0);
        let dup: i64 = r.get::<_, Option<i64>>(20)?.unwrap_or(0);
        let txn_type_str: String = r.get::<_, Option<String>>(14)?.unwrap_or_default();
        let status_str:   String = r.get::<_, Option<String>>(15)?.unwrap_or_default();
        let import_id: Option<i64> = r.get(1)?;
        Ok(Transaction {
            id:               r.get(0)?,
            import_id,
            bank_name:        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            account_no:       r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            date:             r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            date_ts:          r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            narration:        r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            reference:        r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            debit:            r.get(8)?,
            credit:           r.get(9)?,
            balance:          r.get(10)?,
            prev_balance:     r.get(11)?,
            vendor:           r.get::<_, Option<String>>(12)?.unwrap_or_default(),
            account_head:     r.get::<_, Option<String>>(13)?.unwrap_or_default(),
            txn_type:         voucher_from_str(&txn_type_str),
            status:           status_from_str(&status_str),
            confidence:       r.get::<_, Option<f64>>(16)?.unwrap_or(0.0),
            tags,
            balance_ok:       balance_ok_raw.map(|v| v != 0),
            is_opening_balance: is_ob != 0,
            dup_flag:         dup != 0,
        })
    }).context("get_transactions query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("get_transactions collect")
}

pub fn get_transactions_for_import(conn: &Connection, import_id: i64) -> Result<Vec<Transaction>> {
    let mut stmt = conn.prepare(
        "SELECT id, import_id, bank_name, account_no, date, date_ts,
                narration, reference, debit, credit, balance, prev_balance,
                vendor, account_head, txn_type, status, confidence, tags,
                balance_ok, is_opening_bal, dup_flag
         FROM transactions WHERE import_id = ?1
         ORDER BY date_ts ASC, rowid ASC"
    ).context("get_transactions_for_import prepare")?;
    let rows = stmt.query_map(rusqlite::params![import_id], |r| {
        let tags_json: Option<String> = r.get(17)?;
        let tags: Vec<String> = tags_json
            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
            .unwrap_or_default();
        let balance_ok_raw: Option<i64> = r.get(18)?;
        let is_ob: i64 = r.get::<_, Option<i64>>(19)?.unwrap_or(0);
        let dup: i64 = r.get::<_, Option<i64>>(20)?.unwrap_or(0);
        let txn_type_str: String = r.get::<_, Option<String>>(14)?.unwrap_or_default();
        let status_str:   String = r.get::<_, Option<String>>(15)?.unwrap_or_default();
        let import_id_col: Option<i64> = r.get(1)?;
        Ok(Transaction {
            id:               r.get(0)?,
            import_id:        import_id_col,
            bank_name:        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            account_no:       r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            date:             r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            date_ts:          r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            narration:        r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            reference:        r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            debit:            r.get(8)?,
            credit:           r.get(9)?,
            balance:          r.get(10)?,
            prev_balance:     r.get(11)?,
            vendor:           r.get::<_, Option<String>>(12)?.unwrap_or_default(),
            account_head:     r.get::<_, Option<String>>(13)?.unwrap_or_default(),
            txn_type:         voucher_from_str(&txn_type_str),
            status:           status_from_str(&status_str),
            confidence:       r.get::<_, Option<f64>>(16)?.unwrap_or(0.0),
            tags,
            balance_ok:       balance_ok_raw.map(|v| v != 0),
            is_opening_balance: is_ob != 0,
            dup_flag:         dup != 0,
        })
    }).context("get_transactions_for_import query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("get_transactions_for_import collect")
}

pub fn delete_transactions_for_client(conn: &Connection, client_id: i64) -> Result<()> {
    conn.execute("DELETE FROM transactions WHERE client_id = ?1", rusqlite::params![client_id])
        .context("delete_transactions_for_client")?;
    Ok(())
}

pub fn upsert_transaction_classification(
    conn: &Connection,
    txn_id: &str,
    vendor: &str,
    account_head: &str,
    txn_type: &str,
    status: &str,
    confidence: f64,
) -> Result<()> {
    conn.execute(
        "UPDATE transactions SET vendor=?1, account_head=?2, txn_type=?3, status=?4, confidence=?5
         WHERE id=?6",
        rusqlite::params![vendor, account_head, txn_type, status, confidence, txn_id],
    ).context("upsert_transaction_classification")?;
    Ok(())
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

// ── Audit log CRUD ────────────────────────────────────────────────────────────

pub fn push_audit_event(conn: &Connection, client_id: i64, event: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO audit_log (client_id, event) VALUES (?1, ?2)",
        rusqlite::params![client_id, event],
    ).context("push_audit_event")?;
    Ok(())
}

pub fn get_audit_events(conn: &Connection, client_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT event FROM audit_log WHERE client_id = ?1 ORDER BY id DESC LIMIT 500"
    ).context("get_audit_events prepare")?;
    let rows = stmt.query_map(rusqlite::params![client_id], |r| r.get(0))
        .context("get_audit_events query")?;
    rows.collect::<rusqlite::Result<Vec<String>>>().context("get_audit_events collect")
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
    // Additive migrations: ignore errors (column/table already exists)
    let _ = conn.execute(
        "ALTER TABLE transactions ADD COLUMN dup_flag INTEGER NOT NULL DEFAULT 0", [],
    );
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_log (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            client_id  INTEGER NOT NULL DEFAULT 0,
            event      TEXT    NOT NULL,
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_audit_client ON audit_log(client_id, id DESC);"
    );
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
