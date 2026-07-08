// db/mod.rs — SQLite persistence layer with full CRUD.

mod encryption;
pub use encryption::diagnostics;
// Test-only: every test anywhere in the crate that opens a real (non
// `:memory:`) database file touches the same OS-keyring-backed encryption
// key, so all such tests must serialize on this one lock or they race each
// other (a concurrently-running test regenerating/rotating the shared
// keyring entry mid-encrypt/decrypt in another test — this is exactly what
// caused an intermittent "unreadable with the stored key" failure in
// `migration`'s tests once they started running alongside the rest of the
// suite instead of in isolation). Re-exported `pub(crate)` since the lock
// itself lives in the private `encryption` submodule, which isn't reachable
// from sibling modules like `migration` otherwise.
#[cfg(test)]
pub(crate) use encryption::ENCRYPTION_KEYRING_TEST_LOCK;

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
                classified_by, tags, balance_ok, is_opening_bal, dup_flag,
                gst_rate, gst_amount, gst_type
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18,
                ?19, ?20, ?21, ?22, ?23,
                ?24, ?25, ?26
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
                t.gst_rate, t.gst_amount, t.gst_type,
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
                balance_ok, is_opening_bal, dup_flag, classified_by,
                gst_rate, gst_amount, gst_type
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
            gst_rate:         r.get(22)?,
            gst_amount:       r.get(23)?,
            gst_type:         r.get(24)?,
        })
    }).context("get_transactions query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("get_transactions collect")
}

pub fn get_transactions_for_import(conn: &Connection, import_id: i64) -> Result<Vec<Transaction>> {
    let mut stmt = conn.prepare(
        "SELECT id, import_id, bank_name, account_no, date, date_ts,
                narration, reference, debit, credit, balance, prev_balance,
                vendor, account_head, txn_type, status, confidence, tags,
                balance_ok, is_opening_bal, dup_flag, classified_by,
                gst_rate, gst_amount, gst_type
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
            gst_rate:         r.get(22)?,
            gst_amount:       r.get(23)?,
            gst_type:         r.get(24)?,
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

/// `client_id` is required (not optional/inferred) so this can never touch
/// a different client's row that happens to share the same `id` — `id`
/// alone is no longer unique across the whole table (see migration 5),
/// only within a `client_id`, so every write/delete by id must include it.
pub fn upsert_transaction_classification(
    conn: &Connection,
    client_id: i64,
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
         WHERE client_id=?7 AND id=?8",
        rusqlite::params![vendor, account_head, txn_type, status, confidence, classification_source, client_id, txn_id],
    ).context("upsert_transaction_classification")?;
    Ok(())
}

/// `client_id` scopes every row's `WHERE` clause — see
/// `upsert_transaction_classification`'s doc comment for why this is
/// required, not optional, now that `id` alone isn't globally unique.
pub fn update_dup_flags(conn: &Connection, client_id: i64, txns: &[Transaction]) -> Result<()> {
    for t in txns {
        conn.execute(
            "UPDATE transactions SET dup_flag = ?1 WHERE client_id = ?2 AND id = ?3",
            rusqlite::params![if t.dup_flag { 1i64 } else { 0i64 }, client_id, t.id],
        ).context("update_dup_flag")?;
    }
    Ok(())
}

/// `client_id` scopes the `WHERE` clause — see
/// `upsert_transaction_classification`'s doc comment for why this is
/// required, not optional, now that `id` alone isn't globally unique.
pub fn delete_transaction(conn: &Connection, client_id: i64, txn_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM transactions WHERE client_id = ?1 AND id = ?2",
        rusqlite::params![client_id, txn_id],
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

/// Insert a classification rule, silently skipping it if a rule with the same
/// `(client_id, pattern)` already exists (case-insensitively — enforced by
/// `idx_classification_rules_unique`, migration 4). Returns `true` if a new
/// row was actually inserted, `false` if it was a duplicate and got ignored.
pub fn add_rule(
    conn: &Connection, client_id: i64,
    pattern: &str, vendor: &str, account_head: &str, txn_type: &str,
) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO classification_rules (client_id, pattern, vendor, account_head, txn_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![client_id, pattern, vendor, account_head, txn_type],
    ).context("add_rule")?;
    Ok(rows > 0)
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
        // OR IGNORE: a backup file can itself contain two entries for the same
        // (client_id, pattern) — e.g. re-exported after migration 4 landed, or
        // hand-edited — and restoring it must not abort the whole import over
        // one duplicate row (the client's own rules were just wiped above by
        // the DELETE, so there's no pre-existing-row collision to worry about,
        // only intra-file duplicates).
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO classification_rules (client_id, pattern, vendor, account_head, txn_type, priority)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            rusqlite::params![client_id, pattern, vendor, head, typ],
        ).context("import_rules_json insert")?;
        count += inserted;
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
///
/// For real file paths, this transparently encrypts the database at rest
/// (SQLCipher) — see `encryption::open_encrypted` for the one-time
/// migration of a pre-existing plaintext database and the startup recovery
/// path if a previous migration attempt was interrupted. `:memory:` (used
/// throughout the test suite) is intentionally exempt: there is no file to
/// protect, and a large number of existing tests assume a plain, keyless
/// in-memory connection.
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    let conn = if path == Path::new(":memory:") {
        Connection::open(path).with_context(|| format!("Cannot open database at {path:?}"))?
    } else {
        encryption::open_encrypted(path)
            .with_context(|| format!("Cannot open encrypted database at {path:?}"))?
    };

    // Enable WAL mode for better concurrent read performance and crash safety.
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("Failed to set WAL journal mode")?;
    conn.pragma_update(None, "foreign_keys", true)
        .context("Failed to enable foreign keys")?;

    init_schema(&conn).context("Schema initialisation failed")?;

    log::info!("Database opened at {path:?}");
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
    // gst_engine::analyse() has always computed these per transaction, but
    // the result was discarded after a single tag/ledger-fallback use —
    // see PRODUCTION_READINESS_AUDIT_2026-06-22.md Phase 2 item 3.
    (3, "ALTER TABLE transactions ADD COLUMN gst_rate REAL;
         ALTER TABLE transactions ADD COLUMN gst_amount REAL;
         ALTER TABLE transactions ADD COLUMN gst_type TEXT;"),
    // `classification_rules` never had a UNIQUE constraint, so `add_rule`'s
    // `INSERT OR IGNORE` was a silent no-op and duplicate rules (same
    // client_id + pattern, differing only by case) could accumulate without
    // limit — see PRODUCTION_READINESS_AUDIT_2026-06-22.md Phase 2 item 18.
    // The DELETE runs first so a database that already accumulated
    // duplicates before this fix can still have the index created afterwards
    // (SQLite would otherwise refuse to build a UNIQUE index over existing
    // conflicting rows) — it keeps the lowest `id` per (client_id, pattern)
    // group, i.e. the first rule ever learned for that pattern, matching
    // `add_rule`'s original "first one wins" INSERT OR IGNORE intent.
    // COLLATE NOCASE mirrors `apply_rules`'s own case-insensitive matching
    // (`upper.contains(&r.pattern.to_uppercase())`) so "Amazon" and "AMAZON"
    // count as the same rule, not two.
    (4, "DELETE FROM classification_rules
          WHERE id NOT IN (
              SELECT MIN(id) FROM classification_rules
              GROUP BY client_id, pattern COLLATE NOCASE
          );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_classification_rules_unique
             ON classification_rules(client_id, pattern COLLATE NOCASE);"),
    // `transactions.id` was the table's *sole* primary key — globally unique
    // across every client, not scoped per client. Ids are generated purely
    // from in-file row position (e.g. `t_{row_index}_{total_rows}`) with no
    // client or file salt, and the synthetic opening-balance row's id was
    // *unconditionally* the literal string "opening_balance" for every
    // single import, on every pipeline (Excel/PDF/OCR). Since
    // `upsert_transactions` writes via `INSERT OR REPLACE`, any two clients
    // whose imports produced the same id — guaranteed for the
    // opening-balance row, plausible for any two files with matching row
    // counts — silently overwrote and reassigned each other's transactions.
    // See CROSS_CLIENT_TRANSACTION_ID_FIX_REPORT.md for the full writeup.
    //
    // SQLite can't ALTER a PRIMARY KEY in place, so this rebuilds the table
    // with a composite `PRIMARY KEY (client_id, id)` — the schema itself now
    // enforces per-client scoping regardless of how ids are generated,
    // rather than relying on generation-time uniqueness as the only
    // safeguard. Explicit BEGIN/COMMIT (no other migration here needs one —
    // a single ALTER/CREATE is already atomic at the SQLite level, but a
    // multi-statement table rebuild is not) makes this all-or-nothing: a
    // failure partway must not leave the database with a half-built
    // replacement table and no working `transactions` table at all.
    //
    // This migration only stops *future* collisions. Data already lost to a
    // collision before a database is upgraded (a row silently reassigned to
    // the wrong client, e.g. every prior client's opening-balance row bar
    // the last one to write it) cannot be recovered — there is nothing left
    // in the old table to migrate back; by the time this runs, only the
    // most recent writer's version of that row still exists anywhere.
    // Column list matches `upsert_transactions`'s INSERT list exactly, plus
    // `created_at`, which is never written explicitly and always defaults.
    (5, "BEGIN TRANSACTION;
         CREATE TABLE transactions_new (
             id              TEXT    NOT NULL,
             client_id       INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
             import_id       INTEGER REFERENCES import_history(id),
             bank_name       TEXT,
             account_no      TEXT,
             date            TEXT,
             date_ts         INTEGER,
             narration       TEXT,
             reference       TEXT,
             debit           REAL,
             credit          REAL,
             balance         REAL,
             prev_balance    REAL,
             vendor          TEXT,
             account_head    TEXT,
             txn_type        TEXT,
             status          TEXT    NOT NULL DEFAULT 'unreviewed',
             confidence      REAL             DEFAULT 0,
             classified_by   TEXT,
             tags            TEXT,
             balance_ok      INTEGER          DEFAULT 1,
             is_opening_bal  INTEGER          DEFAULT 0,
             dup_flag        INTEGER NOT NULL DEFAULT 0,
             gst_rate        REAL,
             gst_amount      REAL,
             gst_type        TEXT,
             created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
             PRIMARY KEY (client_id, id)
         );
         INSERT INTO transactions_new (
             id, client_id, import_id, bank_name, account_no, date, date_ts,
             narration, reference, debit, credit, balance, prev_balance,
             vendor, account_head, txn_type, status, confidence, classified_by,
             tags, balance_ok, is_opening_bal, dup_flag, gst_rate, gst_amount,
             gst_type, created_at
         )
         SELECT
             id, client_id, import_id, bank_name, account_no, date, date_ts,
             narration, reference, debit, credit, balance, prev_balance,
             vendor, account_head, txn_type, status, confidence, classified_by,
             tags, balance_ok, is_opening_bal, dup_flag, gst_rate, gst_amount,
             gst_type, created_at
         FROM transactions;
         DROP TABLE transactions;
         ALTER TABLE transactions_new RENAME TO transactions;
         CREATE INDEX IF NOT EXISTS idx_txn_client  ON transactions(client_id);
         CREATE INDEX IF NOT EXISTS idx_txn_date_ts ON transactions(date_ts);
         CREATE INDEX IF NOT EXISTS idx_txn_status  ON transactions(status);
         CREATE INDEX IF NOT EXISTS idx_txn_vendor  ON transactions(vendor);
         CREATE INDEX IF NOT EXISTS idx_txn_import  ON transactions(import_id);
         COMMIT;"),
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

    /// The highest version in the real MIGRATIONS list — what any database
    /// (fresh or legacy) should end up at after init_schema, since every
    /// test in this module runs against the real migration list. Avoids
    /// hardcoding a version number that goes stale every time a migration
    /// is appended.
    fn latest_migration_version() -> i64 {
        MIGRATIONS.last().unwrap().0
    }

    #[test]
    fn fresh_database_actually_runs_migrations_and_lands_on_baseline() {
        // A genuinely new database does NOT have dup_flag/audit_log from
        // SCHEMA_SQL alone — they must come from real migrations running,
        // not from being skipped as "already legacy-applied".
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).expect("schema init failed on fresh in-memory DB");
        assert_eq!(user_version(&conn), latest_migration_version());
        assert!(column_exists(&conn, "transactions", "dup_flag").unwrap());
        assert!(column_exists(&conn, "transactions", "gst_rate").unwrap());
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
        // still 0 because nothing ever set it. Migrations newer than the
        // legacy baseline (e.g. migration 3's gst_* columns) are still
        // genuinely new to this database and must still apply.
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
        assert_eq!(user_version(&conn), latest_migration_version());
        assert!(column_exists(&conn, "transactions", "gst_rate").unwrap(),
            "migration 3 is newer than the legacy baseline and must still have applied");
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
    fn migration_4_dedupes_pre_existing_duplicate_rules_before_indexing() {
        // Reproduces the real-world bug: a database created before migration 4
        // existed could already have accumulated duplicate (client_id, pattern)
        // rows, because `add_rule`'s "INSERT OR IGNORE" had no constraint to
        // bounce against. Simulate that state directly (bypassing add_rule),
        // then confirm the migration both cleans up the existing duplicates
        // AND successfully builds the unique index afterwards — if the DELETE
        // step didn't run first, `CREATE UNIQUE INDEX` would fail outright on
        // this data.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        // Actually run migrations 1-3 (not just fake the version number) so
        // this fixture has the real column shape a genuine user_version=3
        // database would have — e.g. migration 1's `transactions.dup_flag` —
        // rather than diverging from reality in a way that only happens to
        // be harmless for whichever migration this test was originally
        // written to exercise.
        apply_migrations(&conn, &MIGRATIONS[..3], 0).unwrap();
        assert_eq!(user_version(&conn), 3, "test setup: must land exactly on version 3");
        let client_id  = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");
        let other_client = add_client(&conn, "Beta Co", "Beta Ledger").expect("add_client 2");

        // Three duplicate rows for the same (client_id, pattern) — including a
        // case-variant — plus one genuinely distinct rule and one belonging to
        // a different client that happens to share the same pattern text.
        conn.execute("INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type) VALUES (?1, 'AMAZON', 'Amazon', 'Office Expense', 'Payment')", rusqlite::params![client_id]).unwrap();
        let dup_id_2: i64 = { conn.execute("INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type) VALUES (?1, 'Amazon', 'Amazon Retail', 'Shopping', 'Payment')", rusqlite::params![client_id]).unwrap(); conn.last_insert_rowid() };
        let dup_id_3: i64 = { conn.execute("INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type) VALUES (?1, 'amazon', 'Amazon Pay', 'Software Expense', 'Payment')", rusqlite::params![client_id]).unwrap(); conn.last_insert_rowid() };
        conn.execute("INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type) VALUES (?1, 'SWIGGY', 'Swiggy', 'Food Expense', 'Payment')", rusqlite::params![client_id]).unwrap();
        conn.execute("INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type) VALUES (?1, 'AMAZON', 'Amazon (other client)', 'Office Expense', 'Payment')", rusqlite::params![other_client]).unwrap();

        let count_before: i64 = conn.query_row("SELECT COUNT(*) FROM classification_rules", [], |r| r.get(0)).unwrap();
        assert_eq!(count_before, 5);

        apply_migrations(&conn, MIGRATIONS, 3).expect("migration 4 must dedupe and index cleanly");
        assert_eq!(user_version(&conn), latest_migration_version());

        let count_after: i64 = conn.query_row("SELECT COUNT(*) FROM classification_rules", [], |r| r.get(0)).unwrap();
        assert_eq!(count_after, 3, "3 AMAZON duplicates for client_id must collapse to 1, leaving AMAZON(client) + SWIGGY(client) + AMAZON(other client)");

        // The lowest id (first-ever-learned) among the 3 duplicates must be the survivor.
        let surviving_id: i64 = conn.query_row(
            "SELECT id FROM classification_rules WHERE client_id = ?1 AND pattern = 'AMAZON' COLLATE NOCASE",
            rusqlite::params![client_id], |r| r.get(0),
        ).unwrap();
        assert!(surviving_id < dup_id_2 && surviving_id < dup_id_3);

        // The other client's row with the same pattern text must have survived untouched.
        let other_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM classification_rules WHERE client_id = ?1",
            rusqlite::params![other_client], |r| r.get(0),
        ).unwrap();
        assert_eq!(other_count, 1, "same pattern under a different client_id is not a duplicate");

        // The unique index must now genuinely reject a fresh duplicate insert attempt.
        let raw_insert = conn.execute(
            "INSERT INTO classification_rules (client_id, pattern, vendor, account_head, txn_type) VALUES (?1, 'AMAZON', 'x', 'y', 'z')",
            rusqlite::params![client_id],
        );
        assert!(raw_insert.is_err(), "unique index must reject a plain duplicate INSERT after migration 4");
    }

    // ── Migration 5: transactions.id scoped per-client ──────────────────────────
    // See CROSS_CLIENT_TRANSACTION_ID_FIX_REPORT.md for the full root-cause
    // writeup. Summary: `transactions.id` used to be the table's sole
    // (globally-unique) primary key, but ids are generated purely from
    // in-file row position with no client-specific salt — two clients whose
    // imports produced the same id silently overwrote and reassigned each
    // other's transactions via `INSERT OR REPLACE`. Migration 5 rebuilds the
    // table with `PRIMARY KEY (client_id, id)`.

    /// Brings a fresh in-memory database to exactly the pre-migration-5
    /// schema shape (migrations 1-4 applied for real, not simulated) so
    /// these tests exercise migration 5 upgrading genuine old-shape data,
    /// not a hand-rolled approximation of it.
    fn db_at_pre_migration_5(conn: &Connection) {
        conn.execute_batch(SCHEMA_SQL).unwrap();
        apply_migrations(conn, &MIGRATIONS[..4], 0).unwrap();
        assert_eq!(user_version(conn), 4, "test setup: must land exactly on version 4, before migration 5");
    }

    #[test]
    fn migration_5_preserves_all_existing_single_client_transactions() {
        let conn = Connection::open_in_memory().unwrap();
        db_at_pre_migration_5(&conn);
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").unwrap();

        // Insert directly against the pre-migration-5 (globally-unique-id)
        // schema, mirroring what a real pre-fix install's data looks like.
        for i in 0..5 {
            conn.execute(
                "INSERT INTO transactions (id, client_id, date, narration, debit, credit)
                 VALUES (?1, ?2, '01/01/2024', ?3, 100.0, NULL)",
                rusqlite::params![format!("t_{i}_5"), client_id, format!("Txn {i}")],
            ).unwrap();
        }
        let count_before: i64 = conn.query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0)).unwrap();
        assert_eq!(count_before, 5);

        apply_migrations(&conn, MIGRATIONS, 4).expect("migration 5 must upgrade existing data cleanly");
        assert_eq!(user_version(&conn), latest_migration_version());

        let txns = get_transactions(&conn, client_id).unwrap();
        assert_eq!(txns.len(), 5, "every pre-existing transaction must survive the rebuild");
        for i in 0..5 {
            assert!(txns.iter().any(|t| t.id == format!("t_{i}_5") && t.narration == format!("Txn {i}")));
        }
    }

    #[test]
    fn migration_5_allows_the_same_literal_id_for_two_different_clients() {
        let conn = Connection::open_in_memory().unwrap();
        db_at_pre_migration_5(&conn);
        let client_a = add_client(&conn, "Client A", "Ledger A").unwrap();

        // Under the pre-migration-5 schema, id is the *sole* primary key —
        // only one row can exist with a given id at a time, so client B's
        // row (added after migration, once creating it no longer collides)
        // is what proves the fix; inserting it *before* migration 5 here
        // would just silently overwrite client A's row via INSERT OR
        // REPLACE, which is the bug itself, not a useful test setup.
        conn.execute(
            "INSERT INTO transactions (id, client_id, date, narration, is_opening_bal)
             VALUES ('opening_balance', ?1, '', 'Opening Balance', 1)",
            rusqlite::params![client_a],
        ).unwrap();

        apply_migrations(&conn, MIGRATIONS, 4).expect("migration 5 must succeed");

        let client_b = add_client(&conn, "Client B", "Ledger B").unwrap();
        // The exact scenario that used to silently corrupt data: a second
        // client writing a transaction with the literal same id as an
        // existing one (the opening-balance row's id is *always* this exact
        // literal, for every client, on every import).
        let inserted = conn.execute(
            "INSERT INTO transactions (id, client_id, date, narration, is_opening_bal)
             VALUES ('opening_balance', ?1, '', 'Opening Balance', 1)",
            rusqlite::params![client_b],
        );
        assert!(inserted.is_ok(), "the same id must be insertable for a different client after migration 5");

        assert_eq!(get_transactions(&conn, client_a).unwrap().len(), 1, "client A's opening-balance row must still exist");
        assert_eq!(get_transactions(&conn, client_b).unwrap().len(), 1, "client B's opening-balance row must also exist");
    }

    #[test]
    fn migration_5_rejects_the_same_id_twice_for_the_same_client() {
        // The composite key is (client_id, id) — a genuine duplicate insert
        // (not INSERT OR REPLACE) for the *same* client and *same* id must
        // still be rejected, exactly as the old single-column id PK did.
        // This proves migration 5 didn't over-correct into no uniqueness
        // constraint at all.
        let conn = Connection::open_in_memory().unwrap();
        db_at_pre_migration_5(&conn);
        apply_migrations(&conn, MIGRATIONS, 4).unwrap();
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").unwrap();

        conn.execute(
            "INSERT INTO transactions (id, client_id, date, narration) VALUES ('t1', ?1, '01/01/2024', 'First')",
            rusqlite::params![client_id],
        ).unwrap();
        let dup = conn.execute(
            "INSERT INTO transactions (id, client_id, date, narration) VALUES ('t1', ?1, '01/01/2024', 'Second')",
            rusqlite::params![client_id],
        );
        assert!(dup.is_err(), "a genuine duplicate INSERT for the same (client_id, id) must still be rejected");
    }

    #[test]
    fn migration_5_is_idempotent_when_run_twice() {
        let conn = Connection::open_in_memory().unwrap();
        db_at_pre_migration_5(&conn);
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").unwrap();
        conn.execute(
            "INSERT INTO transactions (id, client_id, date, narration) VALUES ('t1', ?1, '01/01/2024', 'X')",
            rusqlite::params![client_id],
        ).unwrap();

        apply_migrations(&conn, MIGRATIONS, 4).expect("first run");
        let count_1 = get_transactions(&conn, client_id).unwrap().len();
        // A second call is a no-op (every migration's version is already
        // <= the recorded user_version), matching how `db::open` behaves on
        // every subsequent app launch — must not error or duplicate data.
        apply_migrations(&conn, MIGRATIONS, 4).expect("second run must be a no-op, not an error");
        let count_2 = get_transactions(&conn, client_id).unwrap().len();
        assert_eq!(count_1, count_2, "re-running migrations must not duplicate or lose data");
    }

    #[test]
    fn upsert_transaction_classification_does_not_touch_a_different_clients_row_with_the_same_id() {
        let conn = open(":memory:").expect("open");
        let client_a = add_client(&conn, "Client A", "Ledger A").unwrap();
        let client_b = add_client(&conn, "Client B", "Ledger B").unwrap();
        let txn = Transaction { id: "t1".to_string(), date: "01/01/2024".to_string(), ..Transaction::new("t1") };

        upsert_transactions(&conn, client_a, None, std::slice::from_ref(&txn)).unwrap();
        upsert_transactions(&conn, client_b, None, std::slice::from_ref(&txn)).unwrap();

        upsert_transaction_classification(&conn, client_a, "t1", "Vendor A", "Head A", "Payment", "classified", 0.9, "manual").unwrap();

        let a = get_transactions(&conn, client_a).unwrap();
        let b = get_transactions(&conn, client_b).unwrap();
        assert_eq!(a[0].vendor, "Vendor A", "client A's row must be updated");
        assert_eq!(b[0].vendor, "", "client B's same-id row must be untouched by client A's classification update");
    }

    #[test]
    fn delete_transaction_does_not_touch_a_different_clients_row_with_the_same_id() {
        let conn = open(":memory:").expect("open");
        let client_a = add_client(&conn, "Client A", "Ledger A").unwrap();
        let client_b = add_client(&conn, "Client B", "Ledger B").unwrap();
        let txn = Transaction { id: "t1".to_string(), date: "01/01/2024".to_string(), ..Transaction::new("t1") };

        upsert_transactions(&conn, client_a, None, std::slice::from_ref(&txn)).unwrap();
        upsert_transactions(&conn, client_b, None, std::slice::from_ref(&txn)).unwrap();

        delete_transaction(&conn, client_a, "t1").unwrap();

        assert_eq!(get_transactions(&conn, client_a).unwrap().len(), 0, "client A's row must be deleted");
        assert_eq!(get_transactions(&conn, client_b).unwrap().len(), 1, "client B's same-id row must survive client A's delete");
    }

    #[test]
    fn update_dup_flags_does_not_touch_a_different_clients_row_with_the_same_id() {
        let conn = open(":memory:").expect("open");
        let client_a = add_client(&conn, "Client A", "Ledger A").unwrap();
        let client_b = add_client(&conn, "Client B", "Ledger B").unwrap();
        let txn = Transaction { id: "t1".to_string(), date: "01/01/2024".to_string(), ..Transaction::new("t1") };

        upsert_transactions(&conn, client_a, None, std::slice::from_ref(&txn)).unwrap();
        upsert_transactions(&conn, client_b, None, std::slice::from_ref(&txn)).unwrap();

        let flagged = Transaction { dup_flag: true, ..txn.clone() };
        update_dup_flags(&conn, client_a, &[flagged]).unwrap();

        let a = get_transactions(&conn, client_a).unwrap();
        let b = get_transactions(&conn, client_b).unwrap();
        assert!(a[0].dup_flag, "client A's row must be flagged");
        assert!(!b[0].dup_flag, "client B's same-id row must be untouched by client A's dup-flag update");
    }

    #[test]
    fn add_rule_returns_true_on_first_insert_false_on_duplicate() {
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        let first = add_rule(&conn, client_id, "AMAZON", "Amazon", "Office Expense", "Payment").expect("first add_rule");
        assert!(first, "first insert of a new pattern must report true");

        let second = add_rule(&conn, client_id, "AMAZON", "Different Vendor", "Different Head", "Receipt").expect("second add_rule");
        assert!(!second, "re-adding the same (client_id, pattern) must report false, not duplicate the row");

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM classification_rules WHERE client_id = ?1 AND pattern = 'AMAZON'",
            rusqlite::params![client_id], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 1, "exactly one row must exist despite two add_rule calls");
    }

    #[test]
    fn add_rule_duplicate_detection_is_case_insensitive() {
        // Mirrors `apply_rules`'s own case-insensitive matching
        // (`upper.contains(&r.pattern.to_uppercase())`) — "Amazon" and "AMAZON"
        // are the same rule from the classifier's point of view, so they must
        // be the same row in the database too.
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        assert!(add_rule(&conn, client_id, "Amazon", "Amazon", "Office Expense", "Payment").expect("first"));
        assert!(!add_rule(&conn, client_id, "AMAZON", "x", "y", "z").expect("second"), "case-variant pattern must be treated as a duplicate");
        assert!(!add_rule(&conn, client_id, "amazon", "x", "y", "z").expect("third"), "lowercase variant must also be treated as a duplicate");

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM classification_rules WHERE client_id = ?1",
            rusqlite::params![client_id], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn add_rule_same_pattern_different_client_is_not_a_duplicate() {
        let conn = open(":memory:").expect("open");
        let client_a = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client a");
        let client_b = add_client(&conn, "Beta Co", "Beta Ledger").expect("add_client b");

        assert!(add_rule(&conn, client_a, "AMAZON", "Amazon", "Office Expense", "Payment").expect("client a"));
        assert!(add_rule(&conn, client_b, "AMAZON", "Amazon", "Office Expense", "Payment").expect("client b"),
            "the same pattern for a different client_id is a distinct rule, not a duplicate");
    }

    #[test]
    fn import_rules_json_skips_duplicate_patterns_within_the_same_backup() {
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        let json = r#"{"version":1,"rules":[
            {"pattern":"AMAZON","vendor":"Amazon","account_head":"Office Expense","txn_type":"Payment"},
            {"pattern":"AMAZON","vendor":"Amazon Dup","account_head":"Shopping","txn_type":"Payment"},
            {"pattern":"SWIGGY","vendor":"Swiggy","account_head":"Food Expense","txn_type":"Payment"}
        ]}"#;

        // Must not error out over the duplicate AMAZON entry — restoring a
        // backup that happens to contain a repeated pattern degrades
        // gracefully (keeps the first occurrence) instead of aborting.
        let count = import_rules_json(&conn, client_id, json).expect("import_rules_json must tolerate intra-file duplicates");
        assert_eq!(count, 2, "only the 2 genuinely distinct patterns should be counted as inserted");

        let rules = get_rules(&conn, client_id).expect("get_rules");
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn import_ledgers_inserts_new_entries_and_reports_count() {
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        let entries = vec![
            ("Cash".to_string(), "Cash-in-Hand".to_string()),
            ("HDFC Bank".to_string(), "Bank Accounts".to_string()),
        ];
        let added = import_ledgers(&conn, client_id, &entries).expect("import_ledgers");
        assert_eq!(added, 2, "both new ledger rows should be counted as added");
    }

    #[test]
    fn import_ledgers_skips_duplicates_across_calls() {
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        let first_batch = vec![("Cash".to_string(), "Cash-in-Hand".to_string())];
        let added_first = import_ledgers(&conn, client_id, &first_batch).expect("first import");
        assert_eq!(added_first, 1);

        // Re-importing the same name (e.g. re-running the same file) must not
        // duplicate the row or count it as newly added.
        let second_batch = vec![
            ("Cash".to_string(), "Cash-in-Hand".to_string()),
            ("Sales Account".to_string(), "Sales Accounts".to_string()),
        ];
        let added_second = import_ledgers(&conn, client_id, &second_batch).expect("second import");
        assert_eq!(added_second, 1, "only the genuinely new 'Sales Account' row should count");
    }

    #[test]
    fn import_ledgers_scopes_uniqueness_per_client() {
        let conn = open(":memory:").expect("open");
        let client_a = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client a");
        let client_b = add_client(&conn, "Beta Co", "Beta Ledger").expect("add_client b");

        let entries = vec![("Cash".to_string(), "Cash-in-Hand".to_string())];
        import_ledgers(&conn, client_a, &entries).expect("import for client a");
        let added_b = import_ledgers(&conn, client_b, &entries).expect("import for client b");
        assert_eq!(added_b, 1, "same ledger name for a different client is not a duplicate");
    }

    #[test]
    fn gst_fields_round_trip_through_upsert_and_get() {
        let conn = open(":memory:").expect("open");
        let client_id = add_client(&conn, "Acme Co", "Acme Ledger").expect("add_client");

        let mut t = Transaction::new("t_gst");
        t.date = "01/04/2026".to_string();
        t.narration = "AIRTEL POSTPAID BILL".to_string();
        t.debit = Some(999.0);
        t.gst_rate = Some(18.0);
        t.gst_amount = Some(152.37);
        t.gst_type = Some("CGST+SGST".to_string());

        upsert_transactions(&conn, client_id, None, &[t]).expect("upsert");
        let fetched = get_transactions(&conn, client_id).expect("get_transactions");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].gst_rate, Some(18.0));
        assert_eq!(fetched[0].gst_amount, Some(152.37));
        assert_eq!(fetched[0].gst_type.as_deref(), Some("CGST+SGST"));
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
        // `open()` on a real path now goes through encryption::open_encrypted,
        // which touches the shared OS keyring test entry — hold the same
        // lock encryption::tests uses so the two modules' tests don't race
        // on that one entry.
        let _guard = encryption::ENCRYPTION_KEYRING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!(
            "bsp_migration_smoke_{}.db",
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.bak", path.display()));

        {
            let conn = open(&path).expect("first open of fresh file DB");
            assert_eq!(user_version(&conn), latest_migration_version());
        }
        // Reopen twice more — must stay idempotent, no "duplicate column" etc.
        for _ in 0..2 {
            let conn = open(&path).expect("re-open of existing file DB");
            assert_eq!(user_version(&conn), latest_migration_version());
        }

        let _ = std::fs::remove_file(&path);
    }
}
