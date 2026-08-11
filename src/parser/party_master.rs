//! party_master.rs — Port of `app.js._normalizeVendors()`: two-pass
//! signature-based vendor canonicalization, so that raw narration variants of
//! the same real-world party (different word order, suffixes, etc.) collapse
//! into one canonical ledger name before vendor counts / breakdowns are computed.
//!
//! (`app.js._detectPartyMaster()` — the recurring-parties list — is not ported
//! here since `push_summary_extras()` in main.rs already computes the
//! equivalent "Recurring Parties" breakdown directly from `Transaction::vendor`.)

use std::collections::{HashMap, HashSet};

use super::Transaction;
use crate::narration_cleaner::normalize_ledger_name;

// ── normalize_vendors ─────────────────────────────────────────────────────────

/// Category-head pattern — these strings are accounting category names, not party names.
/// Mirrors `IS_CATEGORY_HEAD` in app.js.
fn is_category_head(s: &str) -> bool {
    let lower = s.to_lowercase();
    [
        "expense",
        "income",
        "payable",
        "receivable",
        "charges",
        "fee",
        "salary",
        "rent",
        "interest",
        "purchase",
        "tax",
        "cash",
        "contra",
        "provision",
        "allowance",
        "sundry debtors",
        "sundry creditors",
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
pub fn normalize_vendors(txns: &mut [Transaction]) -> usize {
    // Pass 1: build signature → canonical map.
    let mut sig_map: HashMap<String, String> = HashMap::new();
    let mut raw_set: HashSet<String> = HashSet::new();

    for t in txns.iter() {
        if t.is_opening_balance {
            continue;
        }
        let v = t.vendor.trim().to_string();
        if !v.is_empty() {
            raw_set.insert(v);
        }

        let h = t.account_head.trim().to_string();
        if h.len() >= 2 && !is_category_head(&h) {
            raw_set.insert(h);
        }
    }

    for raw in &raw_set {
        if raw.len() < 2 {
            continue;
        }
        let canonical = normalize_ledger_name(raw);
        let sig = signature(&canonical);
        sig_map.entry(sig).or_insert_with(|| canonical);
    }

    // Pass 2: apply canonical names.
    let mut changed = 0usize;
    for t in txns.iter_mut() {
        if t.is_opening_balance {
            continue;
        }

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

    // ── normalize_vendors ─────────────────────────────────────────────────────

    #[test]
    fn normalize_vendors_never_touches_the_raw_imported_fields() {
        // Requirement #5's data-integrity rule: vendor canonicalization may
        // rewrite `vendor`/`account_head` (both already system-generated) but
        // must never touch the actual imported row fields, or the Main
        // Screen's black/imported coloring on Date, Narration, Debit,
        // Credit, and Balance would no longer be true.
        let mut txns = vec![Transaction {
            date: "10/04/2024".to_string(),
            narration: "NEFT/AB1234/GAURAV VIDWANS/HDFC0001234".to_string(),
            reference: "AB1234".to_string(),
            debit: Some(2_500.0),
            credit: None,
            balance: Some(9_875.5),
            bank_name: "HDFC Bank".to_string(),
            account_no: "5678XXXXXX1234".to_string(),
            vendor: "GAURAV VIDWANS".to_string(),
            ..Transaction::new("t1")
        }];
        let before = txns[0].clone();

        normalize_vendors(&mut txns);

        assert_ne!(
            txns[0].vendor, before.vendor,
            "sanity check: normalization should actually have changed the vendor here"
        );
        assert_eq!(txns[0].date, before.date);
        assert_eq!(txns[0].narration, before.narration);
        assert_eq!(txns[0].reference, before.reference);
        assert_eq!(txns[0].debit, before.debit);
        assert_eq!(txns[0].credit, before.credit);
        assert_eq!(txns[0].balance, before.balance);
        assert_eq!(txns[0].bank_name, before.bank_name);
        assert_eq!(txns[0].account_no, before.account_no);
    }

    #[test]
    fn normalize_collapses_reversed_names() {
        // "GAURAV VIDWANS" and "VIDWANS GAURAV" → both → "Vidwans Gaurav" (V > G).
        let mut txns = vec![
            Transaction {
                vendor: "GAURAV VIDWANS".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "VIDWANS GAURAV".to_string(),
                ..Transaction::new("t2")
            },
        ];
        normalize_vendors(&mut txns);
        assert_eq!(
            txns[0].vendor, txns[1].vendor,
            "both variants should resolve to the same canonical"
        );
    }

    #[test]
    fn normalize_vendor_dict_applied() {
        let mut txns = vec![
            Transaction {
                vendor: "AMAZON PAY".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "AMAZON INDIA".to_string(),
                ..Transaction::new("t2")
            },
        ];
        normalize_vendors(&mut txns);
        // Both resolve to "Amazon" via VENDOR_DICT.
        assert_eq!(txns[0].vendor, "Amazon");
        assert_eq!(txns[1].vendor, "Amazon");
    }

    // ── Requirement #1 ("Club All Customer / Vendor Names") ─────────────────
    // End-to-end: all five spec-example variants of the same vendor, mixed
    // into one batch alongside an unrelated vendor, must all collapse to one
    // canonical ledger name — and the unrelated vendor must stay untouched.

    #[test]
    fn normalize_vendors_groups_all_spec_example_variants_together() {
        let mut txns = vec![
            Transaction {
                vendor: "ABC Traders".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "ABC TRADERS".to_string(),
                ..Transaction::new("t2")
            },
            Transaction {
                vendor: "ABC Traders Pvt Ltd".to_string(),
                ..Transaction::new("t3")
            },
            Transaction {
                vendor: "A.B.C. Traders".to_string(),
                ..Transaction::new("t4")
            },
            Transaction {
                vendor: "ABC Traders- Mumbai".to_string(),
                ..Transaction::new("t5")
            },
        ];
        normalize_vendors(&mut txns);
        let canon = txns[0].vendor.clone();
        for t in &txns {
            assert_eq!(
                t.vendor, canon,
                "all naming variants of the same vendor must resolve to one canonical name"
            );
        }
    }

    #[test]
    fn normalize_vendors_does_not_merge_unrelated_businesses() {
        let mut txns = vec![
            Transaction {
                vendor: "ABC Traders".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "ABC Traders Pvt Ltd".to_string(),
                ..Transaction::new("t2")
            },
            // Different business entirely — shares the "Traders" business
            // word and even a similar-looking prefix, but is not the same
            // party and must not be folded into the ABC Traders group.
            Transaction {
                vendor: "XYZ Distributors".to_string(),
                ..Transaction::new("t3")
            },
        ];
        normalize_vendors(&mut txns);
        assert_eq!(txns[0].vendor, txns[1].vendor, "ABC Traders variants merge");
        assert_ne!(
            txns[0].vendor, txns[2].vendor,
            "unrelated vendor must not be merged just because it also ends in a business word"
        );
    }

    #[test]
    fn normalize_opening_balance_skipped() {
        let mut txns = vec![Transaction {
            is_opening_balance: true,
            vendor: "OPENING BALANCE".to_string(),
            ..Transaction::new("ob")
        }];
        normalize_vendors(&mut txns);
        assert_eq!(txns[0].vendor, "OPENING BALANCE", "OB rows untouched");
    }

    #[test]
    fn normalize_category_head_skipped() {
        let mut txns = vec![
            Transaction {
                vendor: "IRCTC".to_string(),
                account_head: "Travel Expense".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "IRCTC".to_string(),
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
            Transaction {
                vendor: "RAMESH KUMAR SHARMA".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "RAMESH KUMAR SHARMA".to_string(),
                ..Transaction::new("t2")
            },
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
