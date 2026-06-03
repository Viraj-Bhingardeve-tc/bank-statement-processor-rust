//! party_master.rs — Port of `app.js._detectPartyMaster()` and `app.js._normalizeVendors()`
//!
//! Two public functions:
//!   `detect_party_master(txns)` — returns top-10 recurring parties (≥2 occurrences).
//!   `normalize_vendors(txns)`   — two-pass signature-based vendor canonicalization.

use std::collections::{HashMap, HashSet};

use super::Transaction;
use crate::parser::narration_cleaner::normalize_ledger_name;

// ── PartyMasterEntry ──────────────────────────────────────────────────────────

/// One entry in the party master list.
#[derive(Debug, Clone)]
pub struct PartyMasterEntry {
    pub name:         String,
    pub count:        usize,
    pub total_amount: f64,
}

/// Port of `app.js._detectPartyMaster(txns)`.
///
/// Aggregates `t.vendor` across all non-synthetic transactions,
/// keeps parties that appear ≥ 2 times, sorts by frequency descending,
/// returns up to 10 entries.
pub fn detect_party_master(txns: &[Transaction]) -> Vec<PartyMasterEntry> {
    let mut count:  HashMap<String, usize> = HashMap::new();
    let mut amount: HashMap<String, f64>   = HashMap::new();

    for t in txns {
        if t.is_opening_balance { continue; }
        let p = t.vendor.trim().to_string();
        if p.is_empty() || p.len() < 3 { continue; }
        *count.entry(p.clone()).or_insert(0) += 1;
        *amount.entry(p.clone()).or_insert(0.0) +=
            t.debit.unwrap_or(0.0) + t.credit.unwrap_or(0.0);
    }

    let mut entries: Vec<PartyMasterEntry> = count.into_iter()
        .filter(|(_, c)| *c >= 2)
        .map(|(name, c)| {
            let total = (amount.get(&name).copied().unwrap_or(0.0) * 100.0).round() / 100.0;
            PartyMasterEntry { name, count: c, total_amount: total }
        })
        .collect();

    entries.sort_by(|a, b| b.count.cmp(&a.count).then(b.name.cmp(&a.name)));
    entries.truncate(10);
    entries
}

// ── normalize_vendors ─────────────────────────────────────────────────────────

/// Category-head pattern — these strings are accounting category names, not party names.
/// Mirrors `IS_CATEGORY_HEAD` in app.js.
fn is_category_head(s: &str) -> bool {
    let lower = s.to_lowercase();
    [
        "expense", "income", "payable", "receivable", "charges", "fee",
        "salary", "rent", "interest", "purchase", "tax", "cash", "contra",
        "provision", "allowance", "sundry debtors", "sundry creditors",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

/// Build a signature (order-independent canonical token key) from a normalized name.
/// "Vidwans Gaurav" → "GAURAV|VIDWANS"
fn signature(normalized: &str) -> String {
    let mut tokens: Vec<String> = normalized
        .split_whitespace()
        .map(|t| t.to_uppercase())
        .collect();
    tokens.sort();
    tokens.join("|")
}

/// Port of `app.js._normalizeVendors(txns)`.
///
/// Two-pass algorithm:
///   Pass 1 — For every unique raw vendor/accountHead name:
///             normalize via `normalize_ledger_name` → compute signature →
///             first canonical form seen for that signature wins.
///   Pass 2 — Apply canonical name to `t.vendor` and `t.account_head` for every txn.
///
/// Returns the number of transactions whose `vendor` field was changed.
pub fn normalize_vendors(txns: &mut Vec<Transaction>) -> usize {
    // Pass 1: build signature → canonical map.
    let mut sig_map: HashMap<String, String> = HashMap::new();
    let mut raw_set: HashSet<String> = HashSet::new();

    for t in txns.iter() {
        if t.is_opening_balance { continue; }
        let v = t.vendor.trim().to_string();
        if !v.is_empty() { raw_set.insert(v); }

        let h = t.account_head.trim().to_string();
        if h.len() >= 2 && !is_category_head(&h) { raw_set.insert(h); }
    }

    for raw in &raw_set {
        if raw.len() < 2 { continue; }
        let canonical = normalize_ledger_name(raw);
        let sig = signature(&canonical);
        sig_map.entry(sig).or_insert_with(|| canonical);
    }

    // Pass 2: apply canonical names.
    let mut changed = 0usize;
    for t in txns.iter_mut() {
        if t.is_opening_balance { continue; }

        let v = t.vendor.trim().to_string();
        if !v.is_empty() {
            let canonical = normalize_ledger_name(&v);
            let sig = signature(&canonical);
            if let Some(canon) = sig_map.get(&sig) {
                if t.vendor != *canon {
                    t.vendor = canon.clone();
                    changed += 1;
                }
            }
        }

        let h = t.account_head.trim().to_string();
        if h.len() >= 2 && !is_category_head(&h) {
            let canonical = normalize_ledger_name(&h);
            let sig = signature(&canonical);
            if let Some(canon) = sig_map.get(&sig) {
                if t.account_head != *canon {
                    t.account_head = canon.clone();
                }
            }
        }
    }

    changed
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Transaction;

    fn make_txn(vendor: &str, debit: Option<f64>, credit: Option<f64>) -> Transaction {
        Transaction {
            vendor: vendor.to_string(),
            debit,
            credit,
            ..Transaction::new("t")
        }
    }

    fn make_ob() -> Transaction {
        Transaction { is_opening_balance: true, ..Transaction::new("ob") }
    }

    // ── detect_party_master ───────────────────────────────────────────────────

    #[test]
    fn single_occurrence_excluded() {
        let txns = vec![make_txn("Swiggy", Some(800.0), None)];
        let pm = detect_party_master(&txns);
        assert!(pm.is_empty(), "needs ≥2 occurrences");
    }

    #[test]
    fn two_occurrences_included() {
        let txns = vec![
            make_txn("Swiggy", Some(800.0), None),
            make_txn("Swiggy", Some(650.0), None),
        ];
        let pm = detect_party_master(&txns);
        assert_eq!(pm.len(), 1);
        assert_eq!(pm[0].name, "Swiggy");
        assert_eq!(pm[0].count, 2);
        assert!((pm[0].total_amount - 1450.0).abs() < 0.01);
    }

    #[test]
    fn sorted_by_frequency_descending() {
        let txns = vec![
            make_txn("Amazon",  Some(500.0), None),
            make_txn("Swiggy",  Some(800.0), None),
            make_txn("Swiggy",  Some(650.0), None),
            make_txn("Amazon",  Some(300.0), None),
            make_txn("Amazon",  Some(200.0), None),
        ];
        let pm = detect_party_master(&txns);
        assert_eq!(pm[0].name, "Amazon",  "Amazon (3×) should be first");
        assert_eq!(pm[1].name, "Swiggy",  "Swiggy (2×) should be second");
    }

    #[test]
    fn capped_at_ten() {
        let txns: Vec<Transaction> = (0..12).flat_map(|i| {
            let name = format!("Party{:02}", i);
            vec![make_txn(&name, Some(100.0), None), make_txn(&name, Some(100.0), None)]
        }).collect();
        let pm = detect_party_master(&txns);
        assert!(pm.len() <= 10);
    }

    #[test]
    fn opening_balance_excluded() {
        let txns = vec![
            make_ob(),
            make_txn("Swiggy", Some(800.0), None),
            make_txn("Swiggy", Some(650.0), None),
        ];
        let pm = detect_party_master(&txns);
        assert!(!pm.iter().any(|e| e.name.is_empty()));
    }

    #[test]
    fn short_names_excluded() {
        // name < 3 chars excluded
        let txns = vec![
            make_txn("AB", Some(100.0), None),
            make_txn("AB", Some(100.0), None),
        ];
        let pm = detect_party_master(&txns);
        assert!(pm.is_empty());
    }

    #[test]
    fn total_amount_uses_debit_and_credit() {
        let txns = vec![
            make_txn("IRCTC", Some(1200.0), None),
            make_txn("IRCTC", None, Some(400.0)),  // refund
        ];
        let pm = detect_party_master(&txns);
        assert_eq!(pm.len(), 1);
        assert!((pm[0].total_amount - 1600.0).abs() < 0.01);
    }

    // ── normalize_vendors ─────────────────────────────────────────────────────

    #[test]
    fn normalize_collapses_reversed_names() {
        // "GAURAV VIDWANS" and "VIDWANS GAURAV" → both → "Vidwans Gaurav" (V > G).
        let mut txns = vec![
            Transaction { vendor: "GAURAV VIDWANS".to_string(), ..Transaction::new("t1") },
            Transaction { vendor: "VIDWANS GAURAV".to_string(), ..Transaction::new("t2") },
        ];
        normalize_vendors(&mut txns);
        assert_eq!(txns[0].vendor, txns[1].vendor, "both variants should resolve to the same canonical");
    }

    #[test]
    fn normalize_vendor_dict_applied() {
        let mut txns = vec![
            Transaction { vendor: "AMAZON PAY".to_string(), ..Transaction::new("t1") },
            Transaction { vendor: "AMAZON INDIA".to_string(), ..Transaction::new("t2") },
        ];
        normalize_vendors(&mut txns);
        // Both resolve to "Amazon" via VENDOR_DICT.
        assert_eq!(txns[0].vendor, "Amazon");
        assert_eq!(txns[1].vendor, "Amazon");
    }

    #[test]
    fn normalize_opening_balance_skipped() {
        let mut txns = vec![
            Transaction {
                is_opening_balance: true,
                vendor: "OPENING BALANCE".to_string(),
                ..Transaction::new("ob")
            },
        ];
        normalize_vendors(&mut txns);
        assert_eq!(txns[0].vendor, "OPENING BALANCE", "OB rows untouched");
    }

    #[test]
    fn normalize_category_head_skipped() {
        let mut txns = vec![
            Transaction {
                vendor:       "IRCTC".to_string(),
                account_head: "Travel Expense".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor:       "IRCTC".to_string(),
                account_head: "Travel Expense".to_string(),
                ..Transaction::new("t2")
            },
        ];
        normalize_vendors(&mut txns);
        // "Travel Expense" matches "expense" keyword → not normalized as a party name.
        assert_eq!(txns[0].account_head, "Travel Expense");
    }

    #[test]
    fn normalize_long_name_truncation() {
        // 3-token personal name → capped to 2 by Rule B.
        let mut txns = vec![
            Transaction { vendor: "RAMESH KUMAR SHARMA".to_string(), ..Transaction::new("t1") },
            Transaction { vendor: "RAMESH KUMAR SHARMA".to_string(), ..Transaction::new("t2") },
        ];
        normalize_vendors(&mut txns);
        // After Rule B (no biz word), 2 tokens; Rule C: SHARMA > RAMESH → "Sharma Ramesh"
        // Actually: ["RAMESH","KUMAR","SHARMA"] → Rule B: cap at 2 → ["RAMESH","KUMAR"]
        // Wait, Rule B truncates to the FIRST 2, not the highest-sorted 2.
        // Then Rule C: on 2 tokens RAMESH vs KUMAR: R>K → stays ["RAMESH","KUMAR"].
        // Hmm let me re-check: words[0]="RAMESH", words[1]="KUMAR", words[2]="SHARMA".
        // Rule B: 3 tokens, no biz → truncate(2) → ["RAMESH","KUMAR"].
        // Rule C: words[0].cmp(words[1]) = "RAMESH".cmp("KUMAR") = Greater → no swap.
        // Result: "Ramesh Kumar"
        assert_eq!(txns[0].vendor.split_whitespace().count(), 2);
    }

    // ── signature helper ─────────────────────────────────────────────────────

    #[test]
    fn signature_order_independent() {
        assert_eq!(signature("Vidwans Gaurav"), signature("Gaurav Vidwans"));
    }

    #[test]
    fn is_category_head_matches() {
        assert!(is_category_head("Travel Expense"));
        assert!(is_category_head("rent income"));
        assert!(!is_category_head("IRCTC"));
    }
}
