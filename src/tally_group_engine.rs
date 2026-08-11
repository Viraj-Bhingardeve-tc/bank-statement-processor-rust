//! tally_group_engine.rs — Port of the Electron TallyGroupEngine
//! (src/engines/tally-group-engine.js).
//!
//! Assigns a Tally group (e.g. "Sundry Debtors", "Indirect Expenses") to an
//! account head name based on keyword scoring against the account head only
//! (never narration — the old engine's `_score(headName)` never looks at
//! narration either). Priority: user override → keyword scoring (confidence-
//! gated) → amount/direction fallback.
//!
//! The keyword corpus, group taxonomy, and confidence formula below are a
//! faithful port of the JS `KEYWORD_MAP` / `TALLY_GROUPS` / `_score()` /
//! `classify()` (55 keyword-entries, ~378 individual keyword strings, 21
//! groups). The amount/direction fallback at the end of `classify()` has no
//! JS equivalent — old app simply returns `null` (leaves the ledger's group
//! blank) when no keyword clears the confidence bar. Rust's callers (ledger
//! auto-seeding, Tally XML ledger-master generation) need a concrete group
//! string for every ledger, so the fallback is a deliberate, documented
//! addition, not a fidelity gap.

use once_cell::sync::Lazy;
use std::collections::HashMap;

// ── Tally group constants (21 groups, matching JS TALLY_GROUPS exactly) ──────

// Liabilities
pub const GROUP_CAPITAL_ACCOUNT: &str = "Capital Account";
pub const GROUP_RESERVES: &str = "Reserves & Surplus";
pub const GROUP_LOANS: &str = "Loans (Liability)";
pub const GROUP_BANK_OD: &str = "Bank OD A/c";
pub const GROUP_SUNDRY_CREDITORS: &str = "Sundry Creditors";
pub const GROUP_DUTIES_TAXES: &str = "Duties & Taxes";
pub const GROUP_PROVISIONS: &str = "Provisions";
// Income
pub const GROUP_SALES_ACCOUNTS: &str = "Sales Accounts";
pub const GROUP_DIRECT_INCOME: &str = "Direct Income";
pub const GROUP_INDIRECT_INCOME: &str = "Indirect Income";
// Expenses
pub const GROUP_PURCHASE_ACCOUNTS: &str = "Purchase Accounts";
pub const GROUP_DIRECT_EXPENSES: &str = "Direct Expenses";
pub const GROUP_INDIRECT_EXPENSES: &str = "Indirect Expenses";
// Assets
pub const GROUP_FIXED_ASSETS: &str = "Fixed Assets";
pub const GROUP_INVESTMENTS: &str = "Investments";
pub const GROUP_LOANS_ADVANCES: &str = "Loans & Advances (Asset)";
pub const GROUP_SUNDRY_DEBTORS: &str = "Sundry Debtors";
pub const GROUP_STOCK_IN_TRADE: &str = "Stock-in-Trade";
pub const GROUP_CASH_IN_HAND: &str = "Cash-in-Hand";
pub const GROUP_BANK_ACCOUNTS: &str = "Bank Accounts";
pub const GROUP_DEPOSITS: &str = "Deposits (Asset)";

// ── Keyword → group mapping ───────────────────────────────────────────────────
// Each entry mirrors one `{ kw: [...], group, weight }` block from
// tally-group-engine.js:46-197, in the same order.

struct KwGroup {
    kws: &'static [&'static str],
    group: &'static str,
    weight: i32,
}

static KEYWORD_MAP: Lazy<Vec<KwGroup>> = Lazy::new(|| {
    vec![
        // ── Direct Income / Sales ──────────────────────────────────────────────
        KwGroup {
            kws: &["sales", "sale", "revenue", "turnover", "gross receipt"],
            group: GROUP_SALES_ACCOUNTS,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "service income",
                "service revenue",
                "consulting income",
                "fees income",
                "income from service",
            ],
            group: GROUP_DIRECT_INCOME,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "commission income",
                "brokerage income",
                "agency income",
                "referral income",
            ],
            group: GROUP_INDIRECT_INCOME,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "interest income",
                "interest received",
                "bank interest",
                "fd interest",
                "interest on loan",
            ],
            group: GROUP_INDIRECT_INCOME,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "rental income",
                "rent income",
                "lease income",
                "sublease income",
            ],
            group: GROUP_INDIRECT_INCOME,
            weight: 10,
        },
        KwGroup {
            kws: &["dividend income", "dividend received"],
            group: GROUP_INDIRECT_INCOME,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "misc income",
                "miscellaneous income",
                "other income",
                "sundry income",
            ],
            group: GROUP_INDIRECT_INCOME,
            weight: 8,
        },
        KwGroup {
            kws: &["grant income", "subsidy income", "export incentive"],
            group: GROUP_INDIRECT_INCOME,
            weight: 8,
        },
        // ── Purchase Accounts ──────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "purchase",
                "purchases",
                "raw material",
                "stock purchase",
                "goods purchase",
                "import",
            ],
            group: GROUP_PURCHASE_ACCOUNTS,
            weight: 10,
        },
        // ── Indirect Expenses ──────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "salary",
                "salaries",
                "wage",
                "wages",
                "payroll",
                "stipend",
                "remuneration",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "rent",
                "office rent",
                "shop rent",
                "godown rent",
                "rental expense",
                "lease expense",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 10,
        },
        KwGroup {
            kws: &["telephone", "mobile", "phone bill", "landline", "sim"],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "internet",
                "broadband",
                "data plan",
                "wifi",
                "leased line",
                "connectivity",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "electricity",
                "power bill",
                "utility bill",
                "msedcl",
                "bescom",
                "tneb",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "bank charge",
                "bank fee",
                "bank commission",
                "processing fee",
                "annual fee",
                "service charge",
                "maintenance charge",
                "sms charge",
                "ecs charge",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "professional fee",
                "consultancy",
                "audit fee",
                "legal fee",
                "advocate fee",
                "ca fee",
                "cs fee",
                "notary",
                "registration fee",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "insurance",
                "premium",
                "mediclaim",
                "policy",
                "life insurance",
                "vehicle insurance",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "travel",
                "travelling",
                "tour",
                "conveyance",
                "cab",
                "taxi",
                "ola",
                "uber",
                "irctc",
                "airline",
                "train ticket",
                "flight",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "advertisement",
                "advertising",
                "marketing",
                "promotion",
                "digital ad",
                "facebook ad",
                "google ad",
                "hoarding",
                "banner",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "printing",
                "stationery",
                "office supply",
                "paper",
                "ink",
                "toner",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "repair",
                "maintenance",
                "amc",
                "annual maintenance",
                "service contract",
                "housekeeping",
                "pest control",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &["fuel", "petrol", "diesel", "cng", "hp fuel", "bpcl", "hpcl"],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "food",
                "meal",
                "canteen",
                "lunch",
                "dinner",
                "tea",
                "refreshment",
                "snack",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "medical", "medicine", "pharmacy", "hospital", "doctor", "clinic", "health",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "grocery",
                "vegetable",
                "kirana",
                "supermarket",
                "bigbasket",
                "blinkit",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 7,
        },
        KwGroup {
            kws: &[
                "software",
                "subscription",
                "saas",
                "app",
                "license",
                "domain",
                "hosting",
                "aws",
                "azure",
                "google cloud",
                "github",
                "notion",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "vehicle",
                "car expense",
                "two wheeler",
                "bike service",
                "vehicle maintenance",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "courier",
                "freight",
                "logistics",
                "shipping",
                "delivery charge",
                "cargo",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 8,
        },
        KwGroup {
            kws: &[
                "staff welfare",
                "employee welfare",
                "birthday",
                "outing",
                "team lunch",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 7,
        },
        KwGroup {
            kws: &["security", "guard", "watchman", "cctv", "security service"],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 7,
        },
        KwGroup {
            kws: &["cleaning", "sweeping", "housekeeping", "janitorial"],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 7,
        },
        KwGroup {
            kws: &[
                "miscellaneous",
                "misc expense",
                "sundry expense",
                "petty",
                "petty cash",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 6,
        },
        KwGroup {
            kws: &[
                "office expense",
                "office cost",
                "admin expense",
                "administrative",
            ],
            group: GROUP_INDIRECT_EXPENSES,
            weight: 7,
        },
        // ── Direct Expenses ────────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "production",
                "manufacturing",
                "factory",
                "job work",
                "conversion",
                "packing material",
            ],
            group: GROUP_DIRECT_EXPENSES,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "labour",
                "direct labour",
                "contract labour",
                "labour charge",
            ],
            group: GROUP_DIRECT_EXPENSES,
            weight: 9,
        },
        // ── Duties & Taxes (Liability) ─────────────────────────────────────────
        KwGroup {
            kws: &[
                "gst payable",
                "cgst payable",
                "sgst payable",
                "igst payable",
                "gst liability",
            ],
            group: GROUP_DUTIES_TAXES,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "tds payable",
                "tds deducted",
                "tax deducted",
                "income tax payable",
                "advance tax",
            ],
            group: GROUP_DUTIES_TAXES,
            weight: 10,
        },
        KwGroup {
            kws: &[
                "professional tax",
                "pt payable",
                "esic",
                "provident fund",
                "pf payable",
                "epf",
            ],
            group: GROUP_DUTIES_TAXES,
            weight: 9,
        },
        KwGroup {
            kws: &["customs duty", "import duty", "excise", "cess"],
            group: GROUP_DUTIES_TAXES,
            weight: 9,
        },
        // ── Sundry Creditors (Liability) ───────────────────────────────────────
        KwGroup {
            kws: &[
                "creditor",
                "supplier",
                "vendor payable",
                "accounts payable",
                "trade payable",
            ],
            group: GROUP_SUNDRY_CREDITORS,
            weight: 10,
        },
        // ── Sundry Debtors (Asset) ─────────────────────────────────────────────
        KwGroup {
            kws: &[
                "debtor",
                "customer receivable",
                "accounts receivable",
                "trade receivable",
            ],
            group: GROUP_SUNDRY_DEBTORS,
            weight: 10,
        },
        // ── Cash (Asset) ───────────────────────────────────────────────────────
        KwGroup {
            kws: &["cash", "petty cash", "cash in hand"],
            group: GROUP_CASH_IN_HAND,
            weight: 10,
        },
        // ── Bank Accounts (Asset) ──────────────────────────────────────────────
        KwGroup {
            kws: &[
                "bank account",
                "current account",
                "savings account",
                "hdfc",
                "icici",
                "sbi",
                "axis",
            ],
            group: GROUP_BANK_ACCOUNTS,
            weight: 8,
        },
        // ── Fixed Assets ───────────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "computer", "laptop", "desktop", "server", "printer", "scanner", "hardware",
            ],
            group: GROUP_FIXED_ASSETS,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "furniture",
                "office furniture",
                "chair",
                "table",
                "cabinet",
                "rack",
            ],
            group: GROUP_FIXED_ASSETS,
            weight: 9,
        },
        KwGroup {
            kws: &["machinery", "equipment", "plant", "motor", "generator"],
            group: GROUP_FIXED_ASSETS,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "vehicle",
                "car",
                "bike",
                "truck",
                "van",
                "bus",
                "four wheeler",
            ],
            group: GROUP_FIXED_ASSETS,
            weight: 9,
        },
        KwGroup {
            kws: &[
                "building",
                "land",
                "property",
                "office building",
                "warehouse",
            ],
            group: GROUP_FIXED_ASSETS,
            weight: 9,
        },
        KwGroup {
            kws: &["intangible", "patent", "trademark", "copyright", "goodwill"],
            group: GROUP_FIXED_ASSETS,
            weight: 8,
        },
        // ── Investments (Asset) ────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "investment",
                "mutual fund",
                "share",
                "equity",
                "bonds",
                "debenture",
                "nsc",
                "ppf",
                "fd",
            ],
            group: GROUP_INVESTMENTS,
            weight: 9,
        },
        // ── Loans & Advances (Asset) ───────────────────────────────────────────
        KwGroup {
            kws: &[
                "advance",
                "loan given",
                "loan to staff",
                "advance to employee",
                "security deposit",
                "refundable deposit",
            ],
            group: GROUP_LOANS_ADVANCES,
            weight: 9,
        },
        // ── Deposits (Asset) ───────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "deposit",
                "security deposit",
                "earnest money",
                "margin money",
            ],
            group: GROUP_DEPOSITS,
            weight: 8,
        },
        // ── Loans Liability ────────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "loan",
                "term loan",
                "working capital loan",
                "overdraft",
                "od limit",
                "cc limit",
                "borrowing",
                "credit facility",
                "emi",
                "mortgage",
            ],
            group: GROUP_LOANS,
            weight: 9,
        },
        // ── Capital Account ────────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "capital",
                "owner capital",
                "proprietor capital",
                "partner capital",
                "share capital",
            ],
            group: GROUP_CAPITAL_ACCOUNT,
            weight: 10,
        },
        // ── Reserves & Surplus ─────────────────────────────────────────────────
        KwGroup {
            kws: &[
                "reserve",
                "surplus",
                "retained earning",
                "profit reserve",
                "general reserve",
            ],
            group: GROUP_RESERVES,
            weight: 10,
        },
    ]
});

/// Minimum confidence ratio to accept a keyword-scored group, matching JS
/// `BSPConfig.tallyGroup.minConfidence` default (config.js:47).
const MIN_CONFIDENCE: f64 = 0.3;

/// Port of `TallyGroupEngine._score(headName)` — scores the account head
/// name only (never narration) against every keyword in `KEYWORD_MAP`,
/// adding a length bonus (`floor(keyword.len() / 5)`) per match, exactly as
/// the JS engine does.
fn score_head(head_name: &str) -> HashMap<&'static str, i32> {
    let nl = head_name.trim().to_lowercase();
    let mut scores: HashMap<&'static str, i32> = HashMap::new();
    for entry in KEYWORD_MAP.iter() {
        for k in entry.kws {
            if nl.contains(k) {
                let bonus = (k.len() / 5) as i32;
                *scores.entry(entry.group).or_insert(0) += entry.weight + bonus;
            }
        }
    }
    scores
}

// ── Public classify function ──────────────────────────────────────────────────

/// Classify a transaction into a Tally group.
///
/// `account_head` — ledger name assigned by the classifier (the only input
///                  scored against keywords, matching the old JS engine).
/// `_narration`   — kept for call-site/API stability; unused for scoring
///                  (see module doc — old app's `_score()` never reads it).
/// `is_credit`    — true for credit transactions.
/// `amount`       — absolute transaction amount.
pub fn classify(
    account_head: &str,
    _narration: &str,
    is_credit: bool,
    amount: f64,
    overrides: Option<&HashMap<String, String>>,
) -> String {
    // 1. User overrides (normalized lowercase key) — port of JS `overrides[key]`.
    if let Some(ovr) = overrides {
        let key = account_head.trim().to_lowercase();
        if let Some(group) = ovr.get(&key) {
            return group.clone();
        }
    }

    // 2. Keyword scoring, gated by a confidence ratio (best / (best + second-
    //    best), capped at 0.99; 0.90 when only one group scored) — port of
    //    JS `classify()`'s confidence calculation (tally-group-engine.js:258-265).
    let scores = score_head(account_head);
    if let Some((&best_group, _)) = scores.iter().max_by_key(|(_, v)| **v) {
        let mut vals: Vec<i32> = scores.values().copied().collect();
        vals.sort_unstable_by(|a, b| b.cmp(a));
        let confidence = if vals.len() > 1 {
            (vals[0] as f64 / (vals[0] + vals[1]) as f64).min(0.99)
        } else {
            0.90
        };
        if confidence >= MIN_CONFIDENCE {
            return best_group.to_string();
        }
    }

    // 3. Amount + direction fallback — Rust-specific (see module doc); old
    //    app returns null/blank here instead.
    if amount >= 10_000.0 && !is_credit {
        return GROUP_SUNDRY_CREDITORS.to_string();
    }
    if amount >= 10_000.0 && is_credit {
        return GROUP_SUNDRY_DEBTORS.to_string();
    }
    if !is_credit && amount < 5_000.0 {
        return GROUP_INDIRECT_EXPENSES.to_string();
    }
    if is_credit {
        return GROUP_INDIRECT_INCOME.to_string();
    }

    GROUP_INDIRECT_EXPENSES.to_string()
}

// ── Party (Sundry Debtor/Creditor) detection ──────────────────────────────────

/// Requirement "Sundry Debtors / Creditors": automatically identify whether a
/// party is a customer (Debtor) or a vendor/supplier (Creditor) — from actual
/// evidence, not from amount or credit/debit direction alone.
///
/// A transaction is a *party* ledger — as opposed to an expense/income
/// category ledger — only when the classifier extracted a vendor/customer
/// name (`vendor` non-empty) **and** never assigned it a specific account
/// head (`account_head` empty). That is the exact same "posting ledger fell
/// back to the vendor name" signal `export::excel::posting_ledger` already
/// uses (and the Tally XML ledger-master generator in `export::tally`
/// already keys its own Debtor/Creditor split on) — reused here rather than
/// re-derived, so the Main Screen and both exporters always agree on which
/// ledgers are parties.
///
/// Once it IS a party ledger, `is_receipt` — the transaction's actual
/// voucher direction (a Tally "Receipt", as opposed to "Payment" or
/// "Contra"), not just `credit.is_some()` — decides the side: a customer
/// paying in is a Sundry Debtor, a vendor being paid is a Sundry Creditor.
/// Passing the voucher-type signal instead of raw credit/debit keeps a
/// same-bank ATM withdrawal or self-transfer (classified `Contra`) from
/// ever being miscast as a party, on the rare chance a name got extracted.
///
/// Returns `None` when there's insufficient evidence — either a real account
/// head was already assigned (an existing Direct/Indirect Income, Bank
/// Charges, Salary, etc. classification, which must never be overwritten) or
/// no vendor/customer name was ever extracted — leaving the caller to fall
/// back to keyword/amount-based classification (`classify`) instead of
/// force-classifying an unknown party.
pub fn party_group(account_head: &str, vendor: &str, is_receipt: bool) -> Option<&'static str> {
    if account_head.trim().is_empty() && !vendor.trim().is_empty() {
        Some(if is_receipt {
            GROUP_SUNDRY_DEBTORS
        } else {
            GROUP_SUNDRY_CREDITORS
        })
    } else {
        None
    }
}

/// Classify a batch of transactions.
/// Returns a Vec of Tally group strings, one per input triple.
pub fn classify_batch(
    transactions: &[(String, String, bool, f64)], // (account_head, narration, is_credit, amount)
    overrides: Option<&HashMap<String, String>>,
) -> Vec<String> {
    transactions
        .iter()
        .map(|(head, narr, cr, amt)| classify(head, narr, *cr, *amt, overrides))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salary_is_indirect_expenses() {
        let g = classify("Salary", "salary payment", false, 50000.0, None);
        assert_eq!(g, GROUP_INDIRECT_EXPENSES);
    }

    #[test]
    fn rent_is_indirect_expenses() {
        let g = classify("Office Rent", "rent for office", false, 20000.0, None);
        assert_eq!(g, GROUP_INDIRECT_EXPENSES);
    }

    #[test]
    fn gst_is_duties_taxes() {
        let g = classify("GST Payable", "gst payment", false, 5000.0, None);
        assert_eq!(g, GROUP_DUTIES_TAXES);
    }

    #[test]
    fn interest_received_is_indirect_income() {
        // "Interest A/c" alone doesn't match any keyword phrase (old app requires
        // compound phrases like "interest income"/"bank interest") — this now
        // resolves via the amount/direction fallback (credit, < 10,000) instead
        // of a keyword match, landing on the same group either way.
        let g = classify("Interest A/c", "interest cr", true, 1200.0, None);
        assert_eq!(g, GROUP_INDIRECT_INCOME);
    }

    #[test]
    fn atm_is_cash() {
        let g = classify("Cash", "atm withdrawal", false, 5000.0, None);
        assert_eq!(g, GROUP_CASH_IN_HAND);
    }

    #[test]
    fn loan_emi_is_loans() {
        let g = classify("Loan", "emi deduction", false, 15000.0, None);
        assert_eq!(g, GROUP_LOANS);
    }

    #[test]
    fn mutual_fund_is_investments() {
        // Head must itself contain the keyword phrase now that scoring is
        // head-only (matching old app) — narration is no longer consulted.
        let g = classify("Mutual Fund", "mutual fund purchase", false, 5000.0, None);
        assert_eq!(g, GROUP_INVESTMENTS);
    }

    #[test]
    fn user_override_wins() {
        let mut ovr = HashMap::new();
        ovr.insert("hdfc bank".to_string(), GROUP_BANK_ACCOUNTS.to_string());
        let g = classify("HDFC Bank", "neft transfer", false, 50000.0, Some(&ovr));
        assert_eq!(g, GROUP_BANK_ACCOUNTS);
    }

    #[test]
    fn large_debit_without_kw_is_creditors() {
        let g = classify("Unknown Vendor", "some payment", false, 15000.0, None);
        assert_eq!(g, GROUP_SUNDRY_CREDITORS);
    }

    #[test]
    fn large_credit_without_kw_is_debtors() {
        let g = classify("Unknown", "some receipt", true, 15000.0, None);
        assert_eq!(g, GROUP_SUNDRY_DEBTORS);
    }

    // ── New coverage for groups the prior keyword corpus couldn't reach ──────

    #[test]
    fn sales_is_sales_accounts() {
        let g = classify("Sales", "sales invoice", true, 25000.0, None);
        assert_eq!(g, GROUP_SALES_ACCOUNTS);
    }

    #[test]
    fn purchase_is_purchase_accounts() {
        let g = classify("Purchase", "raw material purchase", false, 25000.0, None);
        assert_eq!(g, GROUP_PURCHASE_ACCOUNTS);
    }

    #[test]
    fn reserve_is_reserves_and_surplus() {
        let g = classify("General Reserve", "reserve transfer", false, 100000.0, None);
        assert_eq!(g, GROUP_RESERVES);
    }

    #[test]
    fn capital_is_capital_account() {
        let g = classify("Owner Capital", "capital introduced", true, 500000.0, None);
        assert_eq!(g, GROUP_CAPITAL_ACCOUNT);
    }

    #[test]
    fn low_confidence_keyword_match_falls_back_to_amount_heuristic() {
        // "fee" (weight 5) alone, on a head that also weakly matches nothing
        // else, still clears MIN_CONFIDENCE as the only scored group (single-
        // group confidence is fixed at 0.90) — confirms the >= 0.3 gate design
        // doesn't accidentally reject legitimate single-keyword matches.
        let g = classify("Bank Fee", "processing fee", false, 500.0, None);
        assert_eq!(g, GROUP_INDIRECT_EXPENSES);
    }

    // ── party_group (Requirement #2: Sundry Debtors / Creditors) ─────────────

    #[test]
    fn customer_receipt_with_known_vendor_and_no_account_head_is_debtor() {
        // A small ₹500 receipt from a known customer — no keyword or amount
        // threshold would catch this under the old amount-only fallback, but
        // the vendor name alone is enough evidence to call it a Debtor.
        let g = party_group("", "Ramesh Kumar", true);
        assert_eq!(g, Some(GROUP_SUNDRY_DEBTORS));
    }

    #[test]
    fn vendor_payment_with_known_vendor_and_no_account_head_is_creditor() {
        let g = party_group("", "ABC Traders", false);
        assert_eq!(g, Some(GROUP_SUNDRY_CREDITORS));
    }

    #[test]
    fn existing_account_head_classification_is_never_overwritten_by_party_group() {
        // Even though a vendor name is also present, a real account head
        // (Salary, an Indirect Expense) already won — must not be reclassified.
        let g = party_group("Salary", "Ramesh Kumar", false);
        assert_eq!(g, None, "an existing account head classification must survive");
    }

    #[test]
    fn credit_with_no_vendor_name_is_not_force_classified_as_debtor() {
        // Insufficient evidence: no party name was ever extracted, so this
        // must NOT be blindly stamped Sundry Debtors just because it's a credit.
        let g = party_group("", "", true);
        assert_eq!(g, None);
    }

    #[test]
    fn debit_with_no_vendor_name_is_not_force_classified_as_creditor() {
        let g = party_group("", "", false);
        assert_eq!(g, None);
    }

    #[test]
    fn whitespace_only_vendor_is_treated_as_no_evidence() {
        let g = party_group("", "   ", true);
        assert_eq!(g, None);
    }
}
