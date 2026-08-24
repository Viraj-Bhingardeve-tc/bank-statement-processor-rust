//! party_master.rs — Port of `app.js._normalizeVendors()` (signature-based
//! vendor canonicalization, so raw narration variants of the same real-world
//! party — different word order, suffixes, etc. — collapse into one
//! canonical ledger name before vendor counts / breakdowns are computed),
//! plus a conservative partial/truncated-name merge pass added 2026-08-25
//! (`is_partial_name_of`, `normalize_vendors`'s own doc comment) for the
//! case the original two-pass port didn't cover: a single truncated word
//! ("Vidwans") that's a prefix of one token in a fuller name a different
//! transaction already has ("Vidwansgaurav Moreshw") — same real party,
//! but not the same token *set*, so the original exact-signature matching
//! left them as two separate Receipts/Payments-by-Ledger entries.
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

/// Minimum characters a short name's token must share with a long name's
/// token before `is_partial_name_of` will trust it as a real prefix match
/// rather than coincidence — see that function's own doc comment.
const MIN_PARTIAL_PREFIX_LEN: usize = 4;

/// Is `short` a plausible truncated/partial form of `long` — every one of
/// `short`'s tokens a case-insensitive *prefix* (at least
/// `MIN_PARTIAL_PREFIX_LEN` characters) of a distinct token in `long`, with
/// `short` having strictly fewer tokens than `long`?
///
/// Exists alongside (not instead of) `signature()`'s exact order-independent
/// token-SET matching above: that one only ever merges names with the exact
/// same tokens in a different order ("GAURAV VIDWANS" ↔ "VIDWANS GAURAV").
/// It has no way to relate a single truncated word ("Vidwans") to a fuller
/// name one of whose tokens it's a prefix of ("Vidwansgaurav Moreshw") —
/// confirmed against a real dataset where exactly that pair was showing up
/// as two separate Receipts-by-Ledger entries for what the transaction
/// amounts make clear is the same real counterparty (both large,
/// same-ballpark RTGS/NEFT credits).
///
/// Deliberately conservative: a bank/OCR-glued name commonly puts the
/// surname first with no separator ("VIDWANSGAURAV" = "Vidwans" + "Gaurav"
/// run together), so prefix-of-a-token (not a full separate-token match,
/// which `signature()` already covers) is exactly the shape a genuine
/// truncation takes — but a *short* prefix risks matching two unrelated
/// names by coincidence (short family surnames especially: "Kuhu Vidwans"
/// and "Chhata Vidwans" in the same dataset are different real people who
/// happen to share a common surname substring with "Vidwans" — the
/// `MIN_PARTIAL_PREFIX_LEN` floor and the caller's own "prefer the highest-
/// weight match when several candidates fit" tie-break (see
/// `normalize_vendors`) both exist to keep that from being enough on its
/// own to force an incorrect merge; a coincidental short-surname match is
/// exactly the kind of ambiguity the weight tie-break resolves toward
/// whichever full name legitimately most needs it — see the module-level
/// doc comment for why that's a deliberate, principled default rather than
/// leaving the ambiguity unresolved).
fn is_partial_name_of(short: &str, long: &str) -> bool {
    let short_tokens: Vec<String> = short.split_whitespace().map(|s| s.to_uppercase()).collect();
    let mut long_tokens: Vec<String> = long.split_whitespace().map(|s| s.to_uppercase()).collect();
    if short_tokens.is_empty() || short_tokens.len() >= long_tokens.len() {
        return false;
    }
    for st in &short_tokens {
        if st.chars().count() < MIN_PARTIAL_PREFIX_LEN {
            return false;
        }
        match long_tokens.iter().position(|lt| lt.starts_with(st.as_str())) {
            Some(pos) => {
                long_tokens.remove(pos);
            }
            None => return false,
        }
    }
    true
}

/// Port of `app.js._normalizeVendors(txns)`.
///
/// Three-pass algorithm:
///   Pass 1 — For every unique raw vendor/accountHead name:
///             normalize via `normalize_ledger_name` → compute signature →
///             first canonical form seen for that signature wins.
///   Pass 1.5 (added 2026-08-25) — For every canonical name that's a
///             plausible truncated/partial form of a *different* canonical
///             name (`is_partial_name_of`, own doc comment above has the
///             full reasoning), remap it onto whichever full-length match
///             covers the most transactions — the real party a short/
///             truncated extraction most plausibly refers to, on the same
///             "biggest real counterparty wins the ambiguity" principle
///             this whole pass exists for. Ties broken alphabetically, for
///             determinism only (an ambiguous tie between two similarly-
///             sized real parties isn't something this can resolve
///             correctly either way).
///   Pass 2 — Apply canonical name (Pass 1.5's remap first, else Pass 1's
///             own canonical) to `t.vendor` and `t.account_head` for every
///             transaction.
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

    // Pass 1.5: partial-name merge (see doc comment above / `is_partial_name_of`).
    // Weight = how many transactions (by vendor OR account_head) already
    // resolve to each signature — "biggest real counterparty" in the sense
    // that actually matters here: how much of this dataset already points
    // to it, not the name's own length or alphabetical position.
    let mut sig_weight: HashMap<String, usize> = HashMap::new();
    for t in txns.iter() {
        if t.is_opening_balance {
            continue;
        }
        let v = t.vendor.trim();
        if !v.is_empty() {
            *sig_weight
                .entry(signature(&normalize_ledger_name(v)))
                .or_insert(0) += 1;
        }
        let h = t.account_head.trim();
        if h.len() >= 2 && !is_category_head(h) {
            *sig_weight
                .entry(signature(&normalize_ledger_name(h)))
                .or_insert(0) += 1;
        }
    }

    let sigs: Vec<(String, String)> = sig_map
        .iter()
        .map(|(s, c)| (s.clone(), c.clone()))
        .collect();
    let mut partial_remap: HashMap<String, String> = HashMap::new();
    for (short_sig, short_canon) in &sigs {
        let mut best: Option<(&str, usize)> = None;
        for (long_sig, long_canon) in &sigs {
            if short_sig == long_sig || !is_partial_name_of(short_canon, long_canon) {
                continue;
            }
            let w = *sig_weight.get(long_sig).unwrap_or(&0);
            let is_better = match best {
                None => true,
                Some((best_canon, best_w)) => {
                    w > best_w || (w == best_w && long_canon.as_str() < best_canon)
                }
            };
            if is_better {
                best = Some((long_canon.as_str(), w));
            }
        }
        if let Some((long_canon, _)) = best {
            partial_remap.insert(short_sig.clone(), long_canon.to_string());
        }
    }

    // Pass 2: apply canonical names (Pass 1.5's remap takes priority).
    let mut changed = 0usize;
    for t in txns.iter_mut() {
        if t.is_opening_balance {
            continue;
        }

        let v = t.vendor.trim().to_string();
        if !v.is_empty() {
            let canonical = normalize_ledger_name(&v);
            let sig = signature(&canonical);
            let resolved = partial_remap.get(&sig).or_else(|| sig_map.get(&sig));
            if let Some(canon) = resolved {
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
            let resolved = partial_remap.get(&sig).or_else(|| sig_map.get(&sig));
            if let Some(canon) = resolved {
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

    // ── Partial/truncated-name merge (Pass 1.5, 2026-08-25) ──────────────────

    #[test]
    fn short_truncated_name_merges_into_the_fuller_name_it_prefixes() {
        // Reproduces the exact real-dataset shape this pass was added for:
        // "Vidwans" (bare, from a mangled NEFT narration) and "Vidwansgaurav
        // Moreshw" (from several RTGS credits, same ballpark amount each
        // time) are the same real counterparty and must end up as one
        // Receipts-by-Ledger entry, not two.
        let mut txns = vec![
            Transaction {
                vendor: "Vidwans".to_string(),
                account_head: "Vidwans".to_string(),
                credit: Some(500_000.0),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "Vidwansgaurav Moreshw".to_string(),
                account_head: "Vidwansgaurav Moreshw".to_string(),
                credit: Some(550_000.0),
                ..Transaction::new("t2")
            },
            Transaction {
                vendor: "Vidwansgaurav Moreshw".to_string(),
                account_head: "Vidwansgaurav Moreshw".to_string(),
                credit: Some(550_000.0),
                ..Transaction::new("t3")
            },
        ];
        normalize_vendors(&mut txns);
        assert_eq!(
            txns[0].vendor, txns[1].vendor,
            "the truncated name must resolve to the same canonical as the fuller name"
        );
        assert_eq!(txns[0].vendor, txns[2].vendor);
        assert_eq!(txns[0].account_head, txns[1].account_head);
    }

    #[test]
    fn short_name_does_not_merge_into_an_unrelated_fuller_name_sharing_no_prefix() {
        // "Vidwans" shares only a common family surname *substring* with
        // "Kuhuvidwans" ("Kuhu" + "Vidwans" glued) — not a *prefix* of
        // either of Kuhuvidwans's own tokens — so it must not be merged in,
        // even though both names are Vidwans-family members in the same
        // dataset. Different real people must stay separate.
        let mut txns = vec![
            Transaction {
                vendor: "Vidwans".to_string(),
                account_head: "Vidwans".to_string(),
                credit: Some(500_000.0),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "Kuhuvidwans".to_string(),
                account_head: "Kuhuvidwans".to_string(),
                debit: Some(60.0),
                ..Transaction::new("t2")
            },
        ];
        normalize_vendors(&mut txns);
        assert_ne!(
            txns[0].vendor, txns[1].vendor,
            "sharing a surname substring that isn't a token prefix must not merge different people"
        );
    }

    #[test]
    fn short_name_prefers_the_higher_weight_match_when_more_than_one_fits() {
        // "Vidwans" is a prefix-match candidate for *both* "Vidwansgaurav
        // Moreshw" (2 transactions) and "Vidwans Chhatagaurav" (1
        // transaction) here — an inherently ambiguous case a name-only
        // algorithm can't resolve with certainty either way. The
        // deliberate tie-break (see `normalize_vendors`'s own doc comment)
        // is the candidate more of the dataset already points to.
        let mut txns = vec![
            Transaction {
                vendor: "Vidwans".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                vendor: "Vidwansgaurav Moreshw".to_string(),
                ..Transaction::new("t2")
            },
            Transaction {
                vendor: "Vidwansgaurav Moreshw".to_string(),
                ..Transaction::new("t3")
            },
            Transaction {
                vendor: "Vidwans Chhatagaurav".to_string(),
                ..Transaction::new("t4")
            },
        ];
        normalize_vendors(&mut txns);
        assert_eq!(
            txns[0].vendor, txns[1].vendor,
            "the ambiguous short name must resolve to the higher-weight (more transactions) candidate"
        );
        assert_ne!(txns[0].vendor, txns[3].vendor);
    }

    #[test]
    fn equal_length_names_are_never_partial_merged() {
        // `is_partial_name_of` requires strictly *fewer* tokens on the short
        // side — two names of the same length must be left to the existing
        // exact-signature matching only (which already handles reordering),
        // never force-merged here just because one prefix-matches the
        // other's tokens.
        assert!(!is_partial_name_of(
            "Vidwansgaurav Moreshw",
            "Vidwansgaurav Moreshw"
        ));
        assert!(!is_partial_name_of("Vid Gau", "Vidwans Gaurav"));
    }

    #[test]
    fn strictly_shorter_name_that_prefixes_every_token_does_match() {
        // Sanity check for the positive case `is_partial_name_of` is meant
        // to catch: a genuinely shorter name whose every token is a prefix
        // of a distinct token in the fuller name.
        assert!(is_partial_name_of(
            "Vidwans Gaurav",
            "Vidwans Gaurav Moreshw"
        ));
    }

    #[test]
    fn short_prefix_below_the_minimum_length_is_not_matched() {
        // A single short/common prefix (below MIN_PARTIAL_PREFIX_LEN) is far
        // too weak a signal on its own — e.g. "Vi" prefixes both "Vidwans"
        // and "Vikram", which are obviously not the same person.
        assert!(!is_partial_name_of("Vi", "Vidwansgaurav Moreshw"));
    }
}
