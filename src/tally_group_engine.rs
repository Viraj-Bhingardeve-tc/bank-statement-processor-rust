//! tally_group_engine.rs — Port of the Electron TallyGroupEngine.
//!
//! Assigns a Tally group (e.g. "Sundry Debtors", "Direct Expenses") to
//! a transaction based on its account_head, narration, and amount.
//!
//! Priority: override map → keyword scoring → amount heuristic → fallback

use once_cell::sync::Lazy;
use std::collections::HashMap;

// ── Tally group constants ────────────────────────────────────────────────────

pub const GROUP_SUNDRY_DEBTORS:    &str = "Sundry Debtors";
pub const GROUP_SUNDRY_CREDITORS:  &str = "Sundry Creditors";
pub const GROUP_BANK_ACCOUNTS:     &str = "Bank Accounts";
pub const GROUP_CASH_IN_HAND:      &str = "Cash-in-Hand";
pub const GROUP_DIRECT_EXPENSES:   &str = "Direct Expenses";
pub const GROUP_INDIRECT_EXPENSES: &str = "Indirect Expenses";
pub const GROUP_DIRECT_INCOME:     &str = "Direct Income";
pub const GROUP_INDIRECT_INCOME:   &str = "Indirect Income";
pub const GROUP_CAPITAL_ACCOUNT:   &str = "Capital Account";
pub const GROUP_LOANS:             &str = "Loans (Liability)";
pub const GROUP_FIXED_ASSETS:      &str = "Fixed Assets";
pub const GROUP_INVESTMENTS:       &str = "Investments";
pub const GROUP_DUTIES_TAXES:      &str = "Duties & Taxes";
pub const GROUP_PROVISIONS:        &str = "Provisions";
pub const GROUP_RESERVES:          &str = "Reserves & Surplus";
pub const GROUP_MISC_EXPENSES:     &str = "Misc. Expenses (Asset)";
pub const GROUP_DEPOSITS:          &str = "Deposits (Asset)";
pub const GROUP_LOANS_ADVANCES:    &str = "Loans & Advances (Asset)";
pub const GROUP_CURRENT_ASSETS:    &str = "Current Assets";
pub const GROUP_CURRENT_LIAB:      &str = "Current Liabilities";
pub const GROUP_SUSPENSE:          &str = "Suspense A/c";

// ── Keyword → (group, weight) map ────────────────────────────────────────────

struct KwEntry {
    group:  &'static str,
    weight: i32,
}

static KEYWORD_MAP: Lazy<Vec<(&'static str, KwEntry)>> = Lazy::new(|| vec![
    // Sundry Debtors
    ("receivable",       KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 9 }),
    ("debtor",           KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 9 }),
    ("customer",         KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 8 }),
    ("client payment",   KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 8 }),
    ("advance received", KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 7 }),
    ("sales receipt",    KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 7 }),
    ("invoice receipt",  KwEntry { group: GROUP_SUNDRY_DEBTORS, weight: 7 }),

    // Sundry Creditors
    ("payable",          KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 9 }),
    ("creditor",         KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 9 }),
    ("vendor",           KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 8 }),
    ("supplier",         KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 8 }),
    ("purchase",         KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 7 }),
    ("advance paid",     KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 7 }),
    ("bill payment",     KwEntry { group: GROUP_SUNDRY_CREDITORS, weight: 6 }),

    // Bank Accounts
    ("transfer",         KwEntry { group: GROUP_BANK_ACCOUNTS, weight: 5 }),
    ("neft",             KwEntry { group: GROUP_BANK_ACCOUNTS, weight: 6 }),
    ("rtgs",             KwEntry { group: GROUP_BANK_ACCOUNTS, weight: 6 }),
    ("imps",             KwEntry { group: GROUP_BANK_ACCOUNTS, weight: 6 }),
    ("upi",              KwEntry { group: GROUP_BANK_ACCOUNTS, weight: 4 }),
    ("internal transfer",KwEntry { group: GROUP_BANK_ACCOUNTS, weight: 8 }),

    // Cash
    ("cash",             KwEntry { group: GROUP_CASH_IN_HAND, weight: 8 }),
    ("atm",              KwEntry { group: GROUP_CASH_IN_HAND, weight: 9 }),
    ("withdrawal",       KwEntry { group: GROUP_CASH_IN_HAND, weight: 7 }),

    // Direct Expenses
    ("raw material",     KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 9 }),
    ("direct cost",      KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 9 }),
    ("labour",           KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 8 }),
    ("labor",            KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 8 }),
    ("manufacturing",    KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 8 }),
    ("production",       KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 7 }),
    ("packaging",        KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 7 }),
    ("freight",          KwEntry { group: GROUP_DIRECT_EXPENSES, weight: 7 }),

    // Indirect Expenses
    ("rent",             KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 9 }),
    ("salary",           KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 9 }),
    ("salaries",         KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 9 }),
    ("wages",            KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 9 }),
    ("utilities",        KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 8 }),
    ("electricity",      KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 8 }),
    ("telephone",        KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("internet",         KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("insurance",        KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 8 }),
    ("repairs",          KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("maintenance",      KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("printing",         KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 6 }),
    ("stationery",       KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 6 }),
    ("postage",          KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 6 }),
    ("advertising",      KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("subscription",     KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 6 }),
    ("fee",              KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 5 }),
    ("fees",             KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 5 }),
    ("charges",          KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 5 }),
    ("commission",       KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("travel",           KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("transport",        KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("fuel",             KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("petrol",           KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),
    ("diesel",           KwEntry { group: GROUP_INDIRECT_EXPENSES, weight: 7 }),

    // Direct Income
    ("sales",            KwEntry { group: GROUP_DIRECT_INCOME, weight: 8 }),
    ("revenue",          KwEntry { group: GROUP_DIRECT_INCOME, weight: 8 }),
    ("service income",   KwEntry { group: GROUP_DIRECT_INCOME, weight: 9 }),
    ("consulting",       KwEntry { group: GROUP_DIRECT_INCOME, weight: 7 }),
    ("export proceeds",  KwEntry { group: GROUP_DIRECT_INCOME, weight: 9 }),

    // Indirect Income
    ("interest income",  KwEntry { group: GROUP_INDIRECT_INCOME, weight: 9 }),
    ("interest earned",  KwEntry { group: GROUP_INDIRECT_INCOME, weight: 9 }),
    ("interest cr",      KwEntry { group: GROUP_INDIRECT_INCOME, weight: 9 }),
    ("dividend",         KwEntry { group: GROUP_INDIRECT_INCOME, weight: 9 }),
    ("rental income",    KwEntry { group: GROUP_INDIRECT_INCOME, weight: 9 }),
    ("commission income",KwEntry { group: GROUP_INDIRECT_INCOME, weight: 8 }),
    ("rebate",           KwEntry { group: GROUP_INDIRECT_INCOME, weight: 6 }),
    ("cashback",         KwEntry { group: GROUP_INDIRECT_INCOME, weight: 6 }),
    ("refund",           KwEntry { group: GROUP_INDIRECT_INCOME, weight: 6 }),
    ("interest",         KwEntry { group: GROUP_INDIRECT_INCOME, weight: 5 }),

    // Capital Account
    ("capital",          KwEntry { group: GROUP_CAPITAL_ACCOUNT, weight: 9 }),
    ("owner",            KwEntry { group: GROUP_CAPITAL_ACCOUNT, weight: 7 }),
    ("proprietor",       KwEntry { group: GROUP_CAPITAL_ACCOUNT, weight: 8 }),
    ("partner capital",  KwEntry { group: GROUP_CAPITAL_ACCOUNT, weight: 9 }),
    ("drawing",          KwEntry { group: GROUP_CAPITAL_ACCOUNT, weight: 8 }),
    ("drawings",         KwEntry { group: GROUP_CAPITAL_ACCOUNT, weight: 8 }),

    // Loans
    ("loan",             KwEntry { group: GROUP_LOANS, weight: 9 }),
    ("emi",              KwEntry { group: GROUP_LOANS, weight: 9 }),
    ("borrowing",        KwEntry { group: GROUP_LOANS, weight: 8 }),
    ("overdraft",        KwEntry { group: GROUP_LOANS, weight: 8 }),
    ("mortgage",         KwEntry { group: GROUP_LOANS, weight: 8 }),
    ("credit card",      KwEntry { group: GROUP_LOANS, weight: 7 }),

    // Fixed Assets
    ("furniture",        KwEntry { group: GROUP_FIXED_ASSETS, weight: 8 }),
    ("equipment",        KwEntry { group: GROUP_FIXED_ASSETS, weight: 8 }),
    ("machinery",        KwEntry { group: GROUP_FIXED_ASSETS, weight: 9 }),
    ("vehicle",          KwEntry { group: GROUP_FIXED_ASSETS, weight: 8 }),
    ("computer",         KwEntry { group: GROUP_FIXED_ASSETS, weight: 7 }),
    ("laptop",           KwEntry { group: GROUP_FIXED_ASSETS, weight: 7 }),
    ("building",         KwEntry { group: GROUP_FIXED_ASSETS, weight: 9 }),

    // Investments
    ("mutual fund",      KwEntry { group: GROUP_INVESTMENTS, weight: 9 }),
    ("equity",           KwEntry { group: GROUP_INVESTMENTS, weight: 7 }),
    ("stock",            KwEntry { group: GROUP_INVESTMENTS, weight: 7 }),
    ("shares",           KwEntry { group: GROUP_INVESTMENTS, weight: 8 }),
    ("investment",       KwEntry { group: GROUP_INVESTMENTS, weight: 8 }),
    ("fd",               KwEntry { group: GROUP_INVESTMENTS, weight: 7 }),
    ("fixed deposit",    KwEntry { group: GROUP_INVESTMENTS, weight: 9 }),
    ("rd",               KwEntry { group: GROUP_INVESTMENTS, weight: 6 }),

    // Duties & Taxes
    ("gst",              KwEntry { group: GROUP_DUTIES_TAXES, weight: 10 }),
    ("tax",              KwEntry { group: GROUP_DUTIES_TAXES, weight: 7 }),
    ("tds",              KwEntry { group: GROUP_DUTIES_TAXES, weight: 9 }),
    ("income tax",       KwEntry { group: GROUP_DUTIES_TAXES, weight: 9 }),
    ("vat",              KwEntry { group: GROUP_DUTIES_TAXES, weight: 8 }),
    ("customs",          KwEntry { group: GROUP_DUTIES_TAXES, weight: 8 }),
    ("igst",             KwEntry { group: GROUP_DUTIES_TAXES, weight: 9 }),
    ("cgst",             KwEntry { group: GROUP_DUTIES_TAXES, weight: 9 }),
    ("sgst",             KwEntry { group: GROUP_DUTIES_TAXES, weight: 9 }),

    // Suspense
    ("suspense",         KwEntry { group: GROUP_SUSPENSE, weight: 9 }),
    ("unknown",          KwEntry { group: GROUP_SUSPENSE, weight: 4 }),
]);

// ── Public classify function ──────────────────────────────────────────────────

/// Classify a transaction into a Tally group.
///
/// `account_head` — ledger name assigned by the classifier
/// `narration`    — raw or cleaned narration string
/// `is_credit`    — true for credit transactions
/// `amount`       — absolute transaction amount
pub fn classify(
    account_head: &str,
    narration: &str,
    is_credit: bool,
    amount: f64,
    overrides: Option<&HashMap<String, String>>,
) -> String {
    // 1. User overrides (normalized lowercase key)
    if let Some(ovr) = overrides {
        let key = account_head.trim().to_lowercase();
        if let Some(group) = ovr.get(&key) {
            return group.clone();
        }
    }

    // 2. Keyword scoring
    let combined = format!("{} {}", account_head, narration).to_lowercase();
    let mut scores: HashMap<&str, i32> = HashMap::new();

    for (kw, entry) in KEYWORD_MAP.iter() {
        if combined.contains(kw) {
            *scores.entry(entry.group).or_insert(0) += entry.weight;
        }
    }

    if let Some((&best_group, &best_score)) = scores.iter().max_by_key(|(_, v)| *v) {
        if best_score >= 5 {
            return best_group.to_string();
        }
    }

    // 3. Amount + direction heuristic
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
        let g = classify("MF", "mutual fund purchase", false, 5000.0, None);
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
}
