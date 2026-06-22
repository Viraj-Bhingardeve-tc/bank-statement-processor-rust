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

/// Upserts all of `txns` atomically: either every row is written, or (on
/// any failure) none are — wrapped in one transaction instead of one
/// implicit autocommit per row, which both avoids leaving a partially
/// applied import on a mid-batch failure and is dramatically faster for
/// multi-thousand-row statements.
pub fn upsert_transactions(
    conn: &Connection,
    client_id: i64,
    import_id: Option<i64>,
    txns: &[Transaction],
) -> Result<usize> {
    let txn = conn.unchecked_transaction().context("upsert_transactions: begin")?;
    let mut count = 0usize;
    for t in txns {
        let tags_json = serde_json::to_string(&t.tags).unwrap_or_else(|_| "[]".to_string());
        let eff_import_id = import_id.or(t.import_id);
        txn.execute(
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
                if t.classification_source.is_empty() { None } else { Some(t.classification_source.clone()) },
                tags_json,
                t.balance_ok.map(|b| if b { 1i64 } else { 0i64 }),
                if t.is_opening_balance { 1i64 } else { 0i64 },
                if t.dup_flag { 1i64 } else { 0i64 },
            ],
        ).context("upsert_transaction row")?;
        count += 1;
    }
    txn.commit().context("upsert_transactions: commit")?;
    Ok(count)
}

pub fn get_transactions(conn: &Connection, client_id: i64) -> Result<Vec<Transaction>> {
    let mut stmt = conn.prepare(
        "SELECT id, import_id, bank_name, account_no, date, date_ts,
                narration, reference, debit, credit, balance, prev_balance,
                vendor, account_head, txn_type, status, confidence, tags,
                balance_ok, is_opening_bal, dup_flag, classified_by
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
        let classification_source: String = r.get::<_, Option<String>>(21)?.unwrap_or_default();
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
            classification_source,
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
                balance_ok, is_opening_bal, dup_flag, classified_by
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
        let classification_source: String = r.get::<_, Option<String>>(21)?.unwrap_or_default();
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
            classification_source,
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
    classification_source: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE transactions SET vendor=?1, account_head=?2, txn_type=?3, status=?4, confidence=?5, classified_by=?6
         WHERE id=?7",
        rusqlite::params![vendor, account_head, txn_type, status, confidence, classification_source, txn_id],
    ).context("upsert_transaction_classification")?;
    Ok(())
}

pub fn update_dup_flags(conn: &Connection, txns: &[Transaction]) -> Result<()> {
    for t in txns {
        conn.execute(
            "UPDATE transactions SET dup_flag = ?1 WHERE id = ?2",
            rusqlite::params![if t.dup_flag { 1i64 } else { 0i64 }, t.id],
        ).context("update_dup_flag")?;
    }
    Ok(())
}

pub fn delete_transaction(conn: &Connection, txn_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM transactions WHERE id = ?1",
        rusqlite::params![txn_id],
    ).context("delete_transaction")?;
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

/// Serialize all rules for `client_id` to a JSON string for backup.
pub fn export_rules_json(conn: &Connection, client_id: i64) -> Result<String> {
    let rules = get_rules(conn, client_id)?;
    let items: Vec<String> = rules.iter().map(|r| {
        format!(
            r#"{{"pattern":{p},"vendor":{v},"account_head":{h},"txn_type":{t}}}"#,
            p = json_str(&r.pattern),
            v = json_str(&r.vendor),
            h = json_str(&r.account_head),
            t = json_str(&r.txn_type),
        )
    }).collect();
    Ok(format!(r#"{{"version":1,"rules":[{}]}}"#, items.join(",")))
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Import rules from a JSON backup string into `client_id`.
/// Existing rules for this client are replaced.
pub fn import_rules_json(conn: &Connection, client_id: i64, json: &str) -> Result<usize> {
    // Minimal hand-rolled parse: find "rules":[...] array and extract objects
    let rules_start = json.find("\"rules\"").and_then(|p| json[p..].find('[').map(|o| p + o + 1));
    if rules_start.is_none() { anyhow::bail!("no 'rules' array in backup JSON"); }
    let rs = rules_start.unwrap();
    // Delete existing client rules first
    conn.execute("DELETE FROM classification_rules WHERE client_id = ?1", rusqlite::params![client_id])
        .context("import_rules_json delete")?;

    let obj_re = regex::Regex::new(r#""pattern"\s*:\s*"([^"]*)"\s*,\s*"vendor"\s*:\s*"([^"]*)"\s*,\s*"account_head"\s*:\s*"([^"]*)"\s*,\s*"txn_type"\s*:\s*"([^"]*)""#).unwrap();
    let slice = &json[rs..];
    let mut count = 0usize;
    for cap in obj_re.captures_iter(slice) {
        let pattern = cap.get(1).map_or("", |m| m.as_str());
        let vendor   = cap.get(2).map_or("", |m| m.as_str());
        let head     = cap.get(3).map_or("", |m| m.as_str());
        let typ      = cap.get(4).map_or("", |m| m.as_str());
        if pattern.is_empty() { continue; }
        conn.execute(
            "INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type, priority)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            rusqlite::params![client_id, pattern, vendor, head, typ],
        ).context("import_rules_json insert")?;
        count += 1;
    }
    Ok(count)
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

pub fn clear_audit_events(conn: &Connection, client_id: i64) -> Result<()> {
    conn.execute("DELETE FROM audit_log WHERE client_id = ?1", rusqlite::params![client_id])
        .context("clear_audit_events")?;
    Ok(())
}

pub fn clear_all_audit_events(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM audit_log", [])
        .context("clear_all_audit_events")?;
    Ok(())
}

// ── Persisted dedup hashes (cross-import duplicate guard) ─────────────────────
// Port of Electron's DB.getDedupeHashes/addDedupeHash/resetDedupeHashes — lets
// dedup catch the same statement being re-loaded across separate import
// sessions, not just duplicate rows within one load.

pub fn get_dedupe_hashes(conn: &Connection, client_id: i64) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT hash FROM dedupe_hashes WHERE client_id = ?1")
        .context("get_dedupe_hashes prepare")?;
    let rows = stmt.query_map(rusqlite::params![client_id], |r| r.get::<_, String>(0))
        .context("get_dedupe_hashes query")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn add_dedupe_hashes(conn: &Connection, client_id: i64, hashes: &[String]) -> Result<()> {
    let txn = conn.unchecked_transaction().context("add_dedupe_hashes: begin")?;
    for h in hashes {
        txn.execute(
            "INSERT OR IGNORE INTO dedupe_hashes (client_id, hash) VALUES (?1, ?2)",
            rusqlite::params![client_id, h],
        ).context("add_dedupe_hashes")?;
    }
    txn.commit().context("add_dedupe_hashes: commit")?;
    Ok(())
}

pub fn reset_dedupe_hashes(conn: &Connection, client_id: i64) -> Result<()> {
    conn.execute("DELETE FROM dedupe_hashes WHERE client_id = ?1", rusqlite::params![client_id])
        .context("reset_dedupe_hashes")?;
    Ok(())
}

// ── Ledger Import ─────────────────────────────────────────────────────────────

/// Insert ledger entries for `client_id`. Each entry is (name, group).
/// Skips duplicates (UNIQUE(client_id, name)). Returns count of newly inserted rows.
pub fn import_ledgers(conn: &Connection, client_id: i64, entries: &[(String, String)]) -> Result<usize> {
    let mut added = 0usize;
    for (name, group) in entries {
        let n = conn.execute(
            "INSERT OR IGNORE INTO ledgers (client_id, name, group_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![client_id, name, group],
        ).context("import_ledgers insert")?;
        added += n;
    }
    Ok(added)
}

/// Automatically seed ledger entries from classified transaction account heads.
/// Skips names that are already present for this client.
pub fn auto_seed_ledgers(conn: &Connection, client_id: i64, heads_with_groups: &[(String, String)]) -> Result<usize> {
    import_ledgers(conn, client_id, heads_with_groups)
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

pub fn delete_setting(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", rusqlite::params![key])
        .context("delete_setting")?;
    Ok(())
}

/// Open (or create) the SQLite database at `path` and run the schema migration.
///
/// A migration failure is fatal and propagates as `Err` rather than being
/// silently swallowed — callers must treat this as a startup-blocking error,
/// not continue with a database that may be missing schema it now expects.
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
//
// Versioned migrations, gated by SQLite's built-in `PRAGMA user_version`.
// Each entry is (version, sql) and is applied at most once, in order, then
// `user_version` is advanced to that version. Once released, an entry's SQL
// must never be edited — only ever append new entries with the next version
// number.
//
// IMPORTANT — historical transition: `user_version` is 0 both on a database
// that has NEVER existed before (just created by `CREATE TABLE` from
// `SCHEMA_SQL` — does not yet have `dup_flag`/`audit_log`) and on a
// pre-existing database created by an older build of this app, which
// already has `dup_flag`/`audit_log` from the old best-effort, error-
// swallowed `ALTER TABLE`/`CREATE IF NOT EXISTS` calls that used to run
// unconditionally on every open (there was no version tracking at all
// before this). Those two `user_version == 0` cases need opposite handling
// — replaying migration 1's `ALTER TABLE ... ADD COLUMN dup_flag` against
// the second case fails with "duplicate column", while skipping it for the
// first case leaves a brand-new database missing the column entirely.
// `user_version` alone cannot tell them apart, so `apply_migrations` below
// checks for the actual presence of `dup_flag` to disambiguate. Do not
// remove that check when adding new migrations.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, "ALTER TABLE transactions ADD COLUMN dup_flag INTEGER NOT NULL DEFAULT 0"),
    (2, "CREATE TABLE IF NOT EXISTS audit_log (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            client_id  INTEGER NOT NULL DEFAULT 0,
            event      TEXT    NOT NULL,
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
         );
         CREATE INDEX IF NOT EXISTS idx_audit_client ON audit_log(client_id, id DESC);"),
];

/// The highest migration version every database created by a pre-migration-
/// framework build of this app already has, unconditionally, IF it already
/// existed before this framework shipped. See the long comment on
/// `MIGRATIONS` above.
const LEGACY_BASELINE_VERSION: i64 = 2;

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("execute_batch failed on base schema SQL")?;
    apply_migrations(conn, MIGRATIONS, LEGACY_BASELINE_VERSION)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        rusqlite::params![column],
        |r| r.get(0),
    ).context("column_exists")?;
    Ok(count > 0)
}

/// Applies every `(version, sql)` pair in `migrations` whose version is
/// greater than the connection's effective baseline, in ascending order,
/// advancing `PRAGMA user_version` after each. Extracted from `init_schema`
/// so the version-arithmetic can be unit-tested directly against a
/// synthetic migration list, without needing a real future migration to
/// exist in `MIGRATIONS` yet.
fn apply_migrations(conn: &Connection, migrations: &[(i64, &str)], legacy_baseline: i64) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .context("read PRAGMA user_version")?;

    let baseline = if current != 0 {
        current
    } else if column_exists(conn, "transactions", "dup_flag")? {
        // user_version == 0 but the legacy migrations' effects are already
        // present -> this is a pre-existing database from before this
        // framework existed, not a brand-new one. Don't replay them.
        legacy_baseline
    } else {
        0
    };

    // user_version is recorded after EACH migration (not just once at the
    // end) so that if a later migration in this same call fails, an
    // earlier one that already succeeded is never replayed on retry.
    let mut last_recorded = current;
    for (version, sql) in migrations.iter().filter(|(v, _)| *v > baseline) {
        conn.execute_batch(sql)
            .with_context(|| format!("migration {version} failed"))?;
        conn.pragma_update(None, "user_version", *version)
            .with_context(|| format!("failed to record user_version {version}"))?;
        last_recorded = *version;
        log::info!("[db] applied migration {version}");
    }

    // Legacy pre-existing DB: `baseline` jumped straight to `legacy_baseline`
    // but nothing in the loop above ran to record that version (no pending
    // migration exceeded it). Record it explicitly so user_version settles
    // instead of re-deriving the baseline via column_exists on every open.
    if last_recorded < baseline {
        conn.pragma_update(None, "user_version", baseline)
            .context("failed to record legacy baseline version")?;
    }

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

-- ── Persisted dedup hashes (cross-import duplicate guard) ────────────────────
CREATE TABLE IF NOT EXISTS dedupe_hashes (
    client_id   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    hash        TEXT    NOT NULL,
    added_at    TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (client_id, hash)
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

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fresh_database_actually_runs_migrations_and_lands_on_baseline() {
        // A genuinely new database does NOT have dup_flag/audit_log from
        // SCHEMA_SQL alone — they must come from real migrations running,
        // not from being skipped as "already legacy-applied".
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).expect("schema init failed on fresh in-memory DB");
        assert_eq!(user_version(&conn), LEGACY_BASELINE_VERSION);
        assert!(column_exists(&conn, "transactions", "dup_flag").unwrap());
        let audit_log_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='audit_log'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(audit_log_exists, 1, "audit_log must have been created by migration 2");
    }

    #[test]
    fn pre_existing_unversioned_db_does_not_replay_legacy_migrations() {
        // Simulate a real pre-migration-framework database: base schema
        // already applied, dup_flag/audit_log already present from the old
        // best-effort ALTER/CREATE-IF-NOT-EXISTS calls, but user_version
        // still 0 because nothing ever set it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute("ALTER TABLE transactions ADD COLUMN dup_flag INTEGER NOT NULL DEFAULT 0", []).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT, client_id INTEGER NOT NULL DEFAULT 0,
                event TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        ).unwrap();
        assert_eq!(user_version(&conn), 0, "precondition: never versioned");

        // Must NOT error with "duplicate column dup_flag".
        init_schema(&conn).expect("init_schema must not replay already-applied legacy migrations");
        assert_eq!(user_version(&conn), LEGACY_BASELINE_VERSION);
    }

    #[test]
    fn forward_migration_applies_once_from_a_versioned_baseline() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();

        let synthetic: &[(i64, &str)] = &[
            (1, "ALTER TABLE transactions ADD COLUMN dup_flag INTEGER NOT NULL DEFAULT 0"),
            (2, "CREATE TABLE IF NOT EXISTS audit_log (id INTEGER PRIMARY KEY);"),
            (3, "ALTER TABLE transactions ADD COLUMN test_marker TEXT"),
        ];
        apply_migrations(&conn, synthetic, 2).expect("migration 3 should apply cleanly");
        assert_eq!(user_version(&conn), 3);

        let has_column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('transactions') WHERE name='test_marker'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(has_column, 1, "migration 3's column must have been added");

        // Re-applying from the now-current version must be a no-op, not an error.
        apply_migrations(&conn, synthetic, 2).expect("re-running init must not replay migration 3");
        assert_eq!(user_version(&conn), 3);
    }

    #[test]
    fn legacy_db_with_nothing_beyond_baseline_still_settles_to_a_recorded_version() {
        // Reproduces "this app shipped only migrations 1 and 2, and a real
        // pre-existing (legacy, dup_flag already present) database is
        // opened" — the loop body has nothing to run (nothing exceeds
        // legacy_baseline), but user_version must still move off 0 so this
        // doesn't re-derive the baseline via column_exists on every future
        // open.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute("ALTER TABLE transactions ADD COLUMN dup_flag INTEGER NOT NULL DEFAULT 0", []).unwrap();

        let synthetic: &[(i64, &str)] = &[
            (1, "ALTER TABLE transactions ADD COLUMN dup_flag INTEGER NOT NULL DEFAULT 0"),
            (2, "CREATE TABLE IF NOT EXISTS audit_log (id INTEGER PRIMARY KEY);"),
        ];
        apply_migrations(&conn, synthetic, 2).expect("must not replay 1/2 against a legacy DB");
        assert_eq!(user_version(&conn), 2, "user_version must settle, not stay 0 forever");
    }

    #[test]
    fn upsert_transactions_is_atomic_on_mid_batch_failure() {
        // FOREIGN KEY enforcement requires the real open() path (PRAGMA
        // foreign_keys = ON is set per-connection, not via raw schema SQL).
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        let mut good = Transaction::new("t_good");
        good.date = "01/04/2026".to_string();
        good.narration = "Opening".to_string();

        let mut bad = Transaction::new("t_bad");
        bad.date = "02/04/2026".to_string();
        bad.narration = "References a client that does not exist".to_string();
        bad.import_id = Some(999_999); // no such import_history row -> FK violation

        let txns = vec![good, bad];
        let result = upsert_transactions(&conn, client_id, None, &txns);

        assert!(result.is_err(), "batch with an FK-violating row must fail");
        let persisted = get_transactions(&conn, client_id).expect("get_transactions");
        assert!(
            persisted.is_empty(),
            "the earlier valid row in the same batch must have been rolled back too, found {} row(s)",
            persisted.len(),
        );
    }

    #[test]
    fn add_dedupe_hashes_rejects_unknown_client_without_partial_writes() {
        let conn = open(":memory:").expect("open");
        // client_id 999_999 doesn't exist -> every row in the batch hits the
        // same FK violation (dedupe_hashes has no other per-row constraint
        // to vary), so this confirms the transaction wrapping doesn't break
        // normal error propagation, while upsert_transactions' test above
        // covers the harder "some rows valid, some not" atomicity case.
        let hashes = vec!["abc12345".to_string(), "deadbeef".to_string()];
        let result = add_dedupe_hashes(&conn, 999_999, &hashes);
        assert!(result.is_err(), "hashes for a non-existent client must fail");
        assert!(get_dedupe_hashes(&conn, 999_999).unwrap().is_empty());
    }

    #[test]
    fn add_dedupe_hashes_happy_path_still_works_under_transaction_wrapping() {
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");
        let hashes = vec!["abc12345".to_string(), "deadbeef".to_string()];
        add_dedupe_hashes(&conn, client_id, &hashes).expect("valid batch must succeed");
        let stored = get_dedupe_hashes(&conn, client_id).unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[test]
    fn real_file_database_opens_idempotently_across_repeated_opens() {
        // Exercises the actual `open()`/`Connection::open` file path (not
        // just open_in_memory), since that's what the running app uses.
        let path = std::env::temp_dir().join(format!(
            "bsp_migration_smoke_{}.db",
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&path);

        {
            let conn = open(&path).expect("first open of fresh file DB");
            assert_eq!(user_version(&conn), LEGACY_BASELINE_VERSION);
        }
        // Reopen twice more — must stay idempotent, no "duplicate column" etc.
        for _ in 0..2 {
            let conn = open(&path).expect("re-open of existing file DB");
            assert_eq!(user_version(&conn), LEGACY_BASELINE_VERSION);
        }

        let _ = std::fs::remove_file(&path);
    }
}
