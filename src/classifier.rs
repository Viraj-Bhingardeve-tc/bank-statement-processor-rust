// classifier.rs — Auto-classification engine.
// Ports App._classify(), _kwMatch(), _detectGST(), _inferVoucherType(),
// _extractPartyName(), and _detectDuplicates() from the original app.js.

use crate::db::ClassificationRule;
use crate::parser::{Transaction, TransactionStatus, VoucherType};
use crate::text_safety::{find_ascii_ci, floor_char_boundary, safe_prefix};

// ── Public entry point ─────────────────────────────────────────────────────────

/// Run full auto-classification pipeline on all transactions in place.
/// Applies stored `rules` first, then falls back to keyword heuristics.
/// `run_dedup`: when false, duplicate detection is skipped.
/// `gst_enabled`: mirrors `Settings.gst_enabled` ("Enable GST detection" in the
/// Settings screen) — when false, the richer `gst_engine::analyse()` pass is
/// skipped entirely (the separate, simpler keyword-based GST/TAX tag from
/// `detect_gst()` is unaffected, matching the old app's own `_detectGST()`
/// behavior, which was never gated by the GST engine's own on/off switch).
/// `gst_auto_ledgers`: mirrors `Settings.gst_auto_ledgers` ("Auto-suggest GST
/// ledgers") — when false, GST rate/amount/type/tag are still computed and
/// surfaced (if `gst_enabled`), but a blank `account_head` is not auto-filled
/// from the GST engine's suggested expense ledger.
/// Returns the number of transactions whose status changed.
pub fn classify_all(
    txns: &mut [Transaction],
    bank_ledger: &str,
    rules: &[ClassificationRule],
    run_dedup: bool,
    gst_enabled: bool,
    gst_auto_ledgers: bool,
) -> usize {
    let mut changed = 0;
    for t in txns.iter_mut() {
        if t.is_opening_balance {
            continue;
        }
        // Don't overwrite user-confirmed rows
        if matches!(t.status, TransactionStatus::Classified) && t.confidence >= 1.0 {
            continue;
        }
        // Don't overwrite AI classifications, even below confidence 1.0 — mirrors
        // Electron's explicit `classifiedBy === 'ai'` guard (app.js:554), since AI
        // confidence comes from the model and isn't guaranteed to be 1.0.
        if matches!(t.status, TransactionStatus::Classified) && t.classification_source == "ai" {
            continue;
        }
        // Don't overwrite suspense
        if matches!(t.status, TransactionStatus::Suspense) {
            continue;
        }

        let before_status = t.status.clone();
        classify_one(t, bank_ledger, rules, gst_enabled, gst_auto_ledgers);
        if t.status != before_status {
            changed += 1;
        }
    }
    if run_dedup {
        detect_duplicates(txns);
    }
    changed
}

/// Apply stored classification rules — returns matched rule or None.
fn apply_rules<'a>(upper: &str, rules: &'a [ClassificationRule]) -> Option<&'a ClassificationRule> {
    rules
        .iter()
        .find(|r| !r.pattern.is_empty() && upper.contains(&r.pattern.to_uppercase()))
}

/// Classify a single transaction: stored rules → keyword heuristics.
fn classify_one(
    t: &mut Transaction,
    bank_ledger: &str,
    rules: &[ClassificationRule],
    gst_enabled: bool,
    gst_auto_ledgers: bool,
) {
    let upper = t.narration.to_uppercase();

    // 1. Stored user rules (highest priority)
    if let Some(rule) = apply_rules(&upper, rules) {
        if !rule.vendor.is_empty() {
            t.vendor = rule.vendor.clone();
        }
        if !rule.account_head.is_empty() {
            t.account_head = rule.account_head.clone();
        }
        if !rule.txn_type.is_empty() {
            t.txn_type = match rule.txn_type.as_str() {
                "Payment" => VoucherType::Payment,
                "Receipt" => VoucherType::Receipt,
                "Contra" => VoucherType::Contra,
                _ => t.txn_type.clone(),
            };
        }
        // Client-scoped rules are more trustworthy than global ones — mirrors
        // Electron's app.js:569-578 (client rule 0.9 vs global rule 0.6).
        t.confidence = if rule.client_id == 0 { 0.6 } else { 0.9 };
        t.status = TransactionStatus::Classified;
        t.classification_source = "rule".to_string();
    // 2. Keyword heuristics
    } else if let Some(kw) = kw_match(&upper, t, bank_ledger) {
        t.vendor = kw.vendor;
        t.account_head = kw.head;
        t.txn_type = kw.txn_type;
        t.confidence = 0.45;
        t.status = TransactionStatus::Classified;
        t.classification_source = "keyword".to_string();
    } else {
        // 3. Extract party name for unreviewed
        let party = extract_party_name(&t.narration);
        if !party.is_empty() {
            t.vendor = party;
        }
        t.status = TransactionStatus::Unreviewed;
        t.confidence = 0.0;
        t.classification_source = String::new();
    }

    // 3. Infer voucher type if missing
    if matches!(t.txn_type, VoucherType::Unknown) {
        t.txn_type = infer_voucher_type(t, &upper);
    }

    // 4. Detect GST/TAX tags (additive)
    if let Some(tag) = detect_gst(&t.narration, &t.reference) {
        if !t.tags.contains(&tag) {
            t.tags.push(tag);
        }
    }

    // 5. Richer GST analysis (rate/vendor-map aware — port of GSTEngine.processBatch),
    // catching GST-applicable vendors by name alone even without an explicit
    // GST/IGST/CGST/SGST keyword, and auto-suggesting an expense ledger when blank.
    // Gated by the Settings screen's "Enable GST detection" toggle.
    if gst_enabled {
        if let Some(gst) =
            crate::gst_engine::analyse(&t.narration, &t.reference, &t.vendor, t.debit, t.credit)
        {
            if !t.tags.contains(&"GST".to_string()) {
                t.tags.push("GST".to_string());
            }
            // "Auto-suggest GST ledgers" toggle — only the ledger auto-fill is
            // gated; rate/amount/type are still surfaced whenever GST detection
            // itself is enabled, since those two settings answer different
            // questions ("detect GST at all?" vs "let it pick my ledger?").
            if gst_auto_ledgers && t.account_head.is_empty() {
                if let Some(ledger) = gst.expense_ledger {
                    t.account_head = ledger;
                }
            }
            // Surface the rest of the analysis instead of discarding it — these
            // used to be computed and immediately dropped (see
            // PRODUCTION_READINESS_AUDIT_2026-06-22.md Phase 2 item 3). Now
            // persisted on the transaction and consumed by export/accounting.rs.
            t.gst_rate = gst.gst_rate;
            t.gst_amount = gst.gst_amount;
            t.gst_type = gst.gst_type;
        }
    }
}

struct KwResult {
    vendor: String,
    head: String,
    txn_type: VoucherType,
}

/// Keyword matching — port of App._kwMatch().
fn kw_match(upper: &str, t: &Transaction, bank_ledger: &str) -> Option<KwResult> {
    macro_rules! kw {
        ($v:expr, $h:expr, $tp:expr, $($k:expr),+) => {
            if [$($k),+].iter().any(|k| upper.contains(k)) {
                return Some(KwResult {
                    vendor:   $v.to_string(),
                    head:     $h.to_string(),
                    txn_type: $tp,
                });
            }
        };
    }

    // ATM / Cash
    kw!(
        "Self",
        "Cash",
        VoucherType::Contra,
        "ATM WDL",
        "ATM-WDL",
        "ATM CASH",
        "ATM/",
        "CASH WITHDRAWAL",
        "CASH WDL",
        "CASH DEP"
    );

    // Bank interest earned
    kw!(
        bank_ledger,
        "Interest Income",
        VoucherType::Receipt,
        "INTEREST CREDITED",
        "INT.CR",
        "INTEREST CR",
        "INT CR",
        "INTEREST CREDIT",
        "FD INTEREST",
        "FD MATURITY",
        "RD MATURITY",
        "INTEREST ON FD"
    );

    // Bank interest charged / bank fees
    kw!(
        bank_ledger,
        "Bank Charges",
        VoucherType::Payment,
        "INTEREST CHARGED",
        "INT.DEB",
        "INTEREST DR",
        "INTEREST DEBIT",
        "BANK CHARGES",
        "SERVICE CHARGE",
        "SMS CHARGES",
        "ANNUAL FEE",
        "LEDGER FOLIO",
        "CHGS RECOVERED",
        "MIN BAL CHGS",
        "PROCESSING FEE",
        "ACCOUNT MAINTENANCE",
        "DEBIT CARD FEE",
        "CREDIT CARD FEE",
        "LOCKER CHARGES"
    );

    // GST payment
    kw!(
        "Government (GST)",
        "GST Payable",
        VoucherType::Payment,
        "GST PAYMENT",
        "GST PMT",
        "IGST PMT",
        "CGST PMT",
        "SGST PMT",
        "NSDL GST",
        "GST CHALLAN"
    );

    // Income Tax / TDS
    kw!(
        "Income Tax Dept",
        "Income Tax Payable",
        VoucherType::Payment,
        "INCOME TAX",
        "ADVANCE TAX",
        "SELF ASSESSMENT TAX",
        "TDS PAYMENT",
        "TDS PMT",
        "CHALLAN 280",
        "ITNS 280",
        "TRACES TDS",
        "TAX DEDUCTED"
    );

    // Salary / Payroll
    kw!(
        "Staff",
        "Salaries",
        VoucherType::Payment,
        "SALARY",
        "SAL/",
        "SAL-",
        "SALARY CREDIT",
        "PAYROLL",
        "WAGES"
    );

    // Rent
    kw!(
        "Landlord",
        "Rent",
        VoucherType::Payment,
        "RENT",
        "RENTAL",
        "LEASE PAYMENT",
        "RENT PAYMENT"
    );

    // Fuel
    kw!(
        "Fuel Station",
        "Fuel Expense",
        VoucherType::Payment,
        "PETROL",
        "DIESEL",
        "FUEL",
        "BPCL",
        "HPCL",
        "IOCL",
        "INDIAN OIL",
        "BHARAT PETRO",
        "SHELL INDIA",
        "HP PUMP",
        "RELIANCE PETRO"
    );

    // Telecom
    if [
        "AIRTEL",
        "VODAFONE",
        "VODAFON",
        "VI/",
        "JIO",
        "BSNL",
        "MTNL",
        "TATASKY",
        "DISH TV",
        "BROADBAND",
        "JIOFIBER",
        "TATA SKY",
    ]
    .iter()
    .any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("AIRTEL") {
            "Airtel"
        } else if upper.contains("JIO") {
            "Jio"
        } else if upper.contains("BSNL") {
            "BSNL"
        } else if upper.contains("VODAFONE") || upper.contains("VI/") {
            "Vodafone"
        } else {
            "Telecom"
        };
        return Some(KwResult {
            vendor: vendor.to_string(),
            head: "Telephone Expense".to_string(),
            txn_type: VoucherType::Payment,
        });
    }

    // Electricity
    kw!(
        "Electricity Board",
        "Electricity Charges",
        VoucherType::Payment,
        "MSEDCL",
        "BESCOM",
        "BSES",
        "TNEB",
        "WBSEDCL",
        "TORRENT POWER",
        "TPDDL",
        "TANGEDCO",
        "ELECTRICITY",
        "ELECTRIC BILL",
        "POWER BILL"
    );

    // Insurance
    kw!(
        "Insurance Co",
        "Insurance Premium",
        VoucherType::Payment,
        "LIC PREMIUM",
        "LIC/",
        "HDFC LIFE",
        "MAX LIFE",
        "ICICI PRU",
        "STAR HEALTH",
        "NIVA BUPA",
        "NEW INDIA ASSURANCE",
        "UNITED INDIA",
        "ORIENTAL INS",
        "INSURANCE PREMIUM",
        "INS PREM"
    );

    // Food delivery
    if ["SWIGGY", "ZOMATO", "UBER EATS", "DUNZO"]
        .iter()
        .any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("SWIGGY") {
            "Swiggy"
        } else if upper.contains("ZOMATO") {
            "Zomato"
        } else {
            "Food Delivery"
        };
        return Some(KwResult {
            vendor: vendor.to_string(),
            head: "Food Expense".to_string(),
            txn_type: VoucherType::Payment,
        });
    }

    // Daily food / restaurants
    kw!(
        "Food / Dining",
        "Food Expense",
        VoucherType::Payment,
        "BREAD",
        "BAKERY",
        "BISCUIT",
        "CANTEEN",
        "TIFFIN",
        "LUNCH",
        "DINNER",
        "BREAKFAST",
        "SNACKS",
        "CHAI",
        "TEA STALL",
        "FOOD",
        "RESTAURANT",
        "HOTEL DINING",
        "MESS",
        "DHABA",
        "CAFE",
        "HALDIRAM",
        "DOMINOS",
        "PIZZA HUT",
        "MCDONALDS",
        "KFC",
        "SUBWAY",
        "BURGER KING",
        "STARBUCKS",
        "CHAAYOS"
    );

    // Grocery
    kw!(
        "Grocery / Kirana",
        "Grocery Expense",
        VoucherType::Payment,
        "BIGBASKET",
        "BIG BAZAAR",
        "DMART",
        "D-MART",
        "RELIANCE FRESH",
        "RELIANCE SMART",
        "MORE MEGASTORE",
        "STAR BAZAAR",
        "JIOMART",
        "GROFERS",
        "BLINKIT",
        "ZEPTO",
        "INSTAMART",
        "MILKBASKET",
        "GROCERY",
        "KIRANA",
        "SABZI",
        "VEGETABLE",
        "FRUITS",
        "DAIRY",
        "MILK",
        "PANEER",
        "GHEE",
        "OIL",
        "RICE",
        "WHEAT",
        "ATTA",
        "PULSES",
        "MASALA",
        "SPICES",
        "PROVISION",
        "BHAJI"
    );

    // Medical / Pharmacy
    kw!(
        "Medical",
        "Medical Expense",
        VoucherType::Payment,
        "MEDPLUS",
        "APOLLO PHARMACY",
        "NETMEDS",
        "TATA 1MG",
        "1MG",
        "PHARMEASY",
        "HEALTHKART",
        "PRACTO",
        "LYBRATE"
    );

    // Online shopping
    if [
        "AMAZON",
        "FLIPKART",
        "MYNTRA",
        "MEESHO",
        "SNAPDEAL",
        "NYKAA",
        "AJIO",
        "SHOPCLUES",
    ]
    .iter()
    .any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("AMAZON") {
            "Amazon"
        } else if upper.contains("FLIPKART") {
            "Flipkart"
        } else if upper.contains("MYNTRA") {
            "Myntra"
        } else if upper.contains("MEESHO") {
            "Meesho"
        } else {
            "Online Shopping"
        };
        return Some(KwResult {
            vendor: vendor.to_string(),
            head: "Office Expense".to_string(),
            txn_type: VoucherType::Payment,
        });
    }

    // Software / SaaS
    if [
        "GOOGLE",
        "MICROSOFT",
        "ZOOM",
        "DROPBOX",
        "CANVA",
        "ADOBE",
        "SLACK",
        "NOTION",
        "GODADDY",
        "BLUEHOST",
        "HOSTINGER",
        "GSUITE",
        "WORKSPACE",
        "OFFICE 365",
        "AWS",
        "AZURE",
        "NETLIFY",
        "VERCEL",
    ]
    .iter()
    .any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("GOOGLE") {
            "Google"
        } else if upper.contains("MICROSOFT") {
            "Microsoft"
        } else if upper.contains("ZOOM") {
            "Zoom"
        } else if upper.contains("AWS") {
            "Amazon AWS"
        } else {
            "Software"
        };
        return Some(KwResult {
            vendor: vendor.to_string(),
            head: "Software Expense".to_string(),
            txn_type: VoucherType::Payment,
        });
    }

    // Ride-hailing
    if ["UBER", "OLA CAB", "OLA/", "RAPIDO", "MERU"]
        .iter()
        .any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("UBER") {
            "Uber"
        } else if upper.contains("OLA") {
            "Ola"
        } else {
            "Transport"
        };
        return Some(KwResult {
            vendor: vendor.to_string(),
            head: "Travelling Expense".to_string(),
            txn_type: VoucherType::Payment,
        });
    }

    // Travel / Hotels / Airlines
    kw!(
        "Travel",
        "Travelling Expense",
        VoucherType::Payment,
        "IRCTC",
        "RAILWAY",
        "INDIGO",
        "AIR INDIA",
        "MAKEMYTRIP",
        "YATRA",
        "CLEARTRIP",
        "GOIBIBO",
        "VISTARA",
        "SPICEJET",
        "AKASA"
    );

    // Professional fees
    kw!(
        "",
        "Professional Fees",
        VoucherType::Payment,
        "PROFESSIONAL FEES",
        "CONSULTING",
        "CA FEES",
        "AUDIT FEES",
        "LEGAL FEES",
        "ADVOCATE FEE",
        "CHARTERED ACCOUNTANT"
    );

    // EMI / Loan
    kw!(
        "Bank/NBFC",
        "Loan Account",
        VoucherType::Payment,
        "EMI",
        "LOAN EMI",
        "HOME LOAN EMI",
        "CAR LOAN",
        "PERSONAL LOAN",
        "BAJAJ FINSERV",
        "HDFC LOAN",
        "ICICI LOAN",
        "SBI LOAN"
    );

    // Investments
    kw!(
        "Investment",
        "Investments",
        VoucherType::Payment,
        "MUTUAL FUND",
        "SIP",
        "ZERODHA",
        "GROWW",
        "UPSTOX",
        "ANGEL BROKING",
        "ICICIDIRECT",
        "HDFC SEC",
        "KOTAK SEC",
        "MOTILAL",
        "NUVAMA",
        "PAYTM MONEY"
    );

    // Dividend
    kw!(
        "Investment",
        "Dividend Income",
        VoucherType::Receipt,
        "DIVIDEND",
        "DIVIDEND CREDIT"
    );

    // Advertisement
    kw!(
        "Advertisement",
        "Advertisement Expense",
        VoucherType::Payment,
        "GOOGLE ADS",
        "FACEBOOK ADS",
        "META ADS",
        "INSTAGRAM ADS",
        "YOUTUBE ADS",
        "DIGITAL MARKETING"
    );

    // NEFT/RTGS/IMPS/UPI — extract party name + direction
    if [
        "NEFT", "INFT", "RTGS", "IMPS", "UPI", "NACH", "ECS", "ACH", "BBPS",
    ]
    .iter()
    .any(|k| upper.contains(k))
    {
        let party = extract_party_name(&t.narration);
        let (head, tp) = if t.credit.is_some() && t.debit.is_none() {
            (
                if party.is_empty() {
                    "Sundry Debtors".to_string()
                } else {
                    party.clone()
                },
                VoucherType::Receipt,
            )
        } else {
            (
                if party.is_empty() {
                    "Sundry Creditors".to_string()
                } else {
                    party.clone()
                },
                VoucherType::Payment,
            )
        };
        return Some(KwResult {
            vendor: party,
            head,
            txn_type: tp,
        });
    }

    None
}

/// Detect GST/TAX tags from narration + reference.
pub fn detect_gst(narration: &str, reference: &str) -> Option<String> {
    let n = format!("{} {}", narration, reference).to_uppercase();
    if n.contains("IGST") || n.contains("CGST") || n.contains("SGST") {
        return Some("GST".to_string());
    }
    if n.contains("GST")
        && (n.contains("PAY") || n.contains("PMT") || n.contains("REFUND") || n.contains("CHALLAN"))
    {
        return Some("GST".to_string());
    }
    // GSTIN pattern: 15-char alphanumeric starting with 2 digits
    let bytes = n.as_bytes();
    for i in 0..bytes.len().saturating_sub(15) {
        // `n` is uppercased narration/reference text, not guaranteed ASCII
        // (₹, accented names, ...) — a valid GSTIN is always pure ASCII, so
        // any `i`/`i+15` that isn't already a char boundary can't possibly
        // be the start of one anyway; skip it instead of panicking on
        // `&n[i..i+15]` (Phase 4L.2.2).
        if !n.is_char_boundary(i) || !n.is_char_boundary(i + 15) {
            continue;
        }
        let chunk = &n[i..i + 15];
        // `i`/`i+15` landing on boundaries only guarantees `chunk` itself is
        // a valid &str — it says nothing about the *internal* cut points
        // (2, 7, 11) the sub-slices below need, which can still fall
        // mid-character even then (Phase 4L.2.2).
        if chunk.len() == 15
            && chunk.is_char_boundary(2)
            && chunk.is_char_boundary(7)
            && chunk.is_char_boundary(11)
            && chunk[..2].chars().all(|c| c.is_ascii_digit())
            && chunk[2..7].chars().all(|c| c.is_ascii_alphabetic())
            && chunk[7..11].chars().all(|c| c.is_ascii_digit())
            && chunk.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Some("GST".to_string());
        }
    }
    if n.contains("INCOME TAX")
        || n.contains("ADVANCE TAX")
        || n.contains("TDS PMT")
        || n.contains("TRACES")
        || n.contains("CHALLAN 28")
    {
        return Some("TAX".to_string());
    }
    None
}

/// Infer voucher type from transaction direction.
pub fn infer_voucher_type(t: &Transaction, upper: &str) -> VoucherType {
    use regex::Regex;
    static ATM_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let atm_re = ATM_RE.get_or_init(|| {
        Regex::new(r"\bATM\b|CASH\s*(DEP|WDL|WITHDRAWAL)|\bSELF\s*TRANSFER\b").unwrap()
    });
    if atm_re.is_match(upper) {
        return VoucherType::Contra;
    }
    if t.credit.is_some() && t.debit.is_none() {
        return VoucherType::Receipt;
    }
    if t.debit.is_some() && t.credit.is_none() {
        return VoucherType::Payment;
    }
    VoucherType::Unknown
}

/// Extract party name from NEFT/RTGS/UPI-style narrations.
pub fn extract_party_name(narration: &str) -> String {
    const SKIP: &[&str] = &[
        "NEFT",
        "RTGS",
        "IMPS",
        "UPI",
        "INFT",
        "NACH",
        "ECS",
        "ACH",
        "BBPS",
        "CR",
        "DR",
        "CREDIT",
        "DEBIT",
        "INWARD",
        "OUTWARD",
        "BY",
        "TO",
        "FROM",
        "VIA",
        "FOR",
        "PER",
        "OF",
        "AT",
        "ON",
        "IN",
        "REF",
        "UTR",
        "TXN",
        "NO",
        "NUMBER",
        "TRF",
        "TRANSFER",
        "PAYMENT",
        "RECEIVED",
        "PAID",
        "SENT",
        "P2P",
        "P2M",
        "P2B",
        "P2A",
        "ONLINE",
        "NET",
        "BANKING",
        "INB",
        "MB",
        "BANK",
        "BRANCH",
        "IFSC",
        "CHQ",
        "CHEQUE",
        "DEP",
        "DEPOSIT",
        "WDL",
        "WITHDRAWAL",
        "WITH",
        "INT",
        "INTEREST",
        "CLG",
        "CLEARING",
        "CL",
        "SB",
        "CA",
        "OD",
        "FD",
        "RD",
        "SAVINGS",
        "CURRENT",
        "A/C",
        "AC",
        "ACCT",
        "ACCOUNT",
        "AMT",
        "AMOUNT",
        "BAL",
        "BALANCE",
        "CHRGS",
        "CHGS",
        "CHARGES",
        "CHARGE",
        "LEVY",
    ];
    const BANK_ABBR: &[&str] = &[
        "HDFC", "HDFCBANK", "ICICI", "ICICIB", "SBI", "SBIN", "AXIS", "AXISB", "KOTAK", "PNB",
        "BOI", "BOB", "IOB", "CANARA", "UNION", "IDBI", "YES", "RBL", "FEDERAL", "INDUSIND", "UCO",
        "PAYTM", "PHONEPE", "GPAY",
    ];

    let is_junk = |s: &str| -> bool {
        let up = s.to_uppercase();
        let up = up.trim();
        if s.len() < 2 {
            return true;
        }
        if SKIP.contains(&up) {
            return true;
        }
        if s.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
        if s.contains('@') {
            return true;
        }
        // IFSC code pattern: 4 alpha + 0 + 6 alphanumeric
        // `s.is_char_boundary(4)`: `s` is a raw narration token, not
        // guaranteed ASCII — a real IFSC code's first 4 chars always are,
        // so a non-boundary at byte 4 already means this isn't one; guards
        // `&s[..4]` from panicking on a token containing a multi-byte
        // character in its first 4 bytes (Phase 4L.2.2).
        if s.len() == 11
            && s.is_char_boundary(4)
            && s[..4].chars().all(|c| c.is_ascii_alphabetic())
            && s.chars().nth(4) == Some('0')
        {
            return true;
        }
        if s.len() >= 14 && s.chars().all(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
        if s.len() <= 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
            return true;
        }
        false
    };

    let is_bank_abbr = |s: &str| BANK_ABBR.contains(&s.to_uppercase().trim());

    // Case 1: delimiter-structured narration
    let parts: Vec<&str> = narration.split(['/', '-', '|', ':']).collect();
    if parts.len() > 1 {
        let mut best: Option<(&str, i32)> = None;
        for p in parts
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && !is_junk(s))
        {
            let words: Vec<&str> = p.split_whitespace().collect();
            let mut score = (words.len() as i32) * 3;
            if p.chars().any(|c| c.is_ascii_digit()) {
                score -= 2;
            }
            if is_bank_abbr(p) || words.first().is_some_and(|w| is_bank_abbr(w)) {
                score -= 6;
            }
            if words.len() == 1 && p.len() > 10 && p.chars().all(|c| c.is_ascii_alphanumeric()) {
                score -= 3;
            }
            if score >= 3 && (best.is_none() || score > best.unwrap().1) {
                best = Some((p, score));
            }
        }
        if let Some((name, _)) = best {
            return normalize_vendor_name(name);
        }
    }

    // Case 2: sentence-style — skip junk words and take first run of valid words
    let words: Vec<&str> = narration.split_whitespace().collect();
    let is_ref_code = |w: &str| {
        w.len() >= 9
            && w.chars().any(|c| c.is_ascii_digit())
            && w.chars().all(|c| c.is_ascii_alphanumeric())
    };
    let start = words.iter().position(|w| {
        let up = w.to_uppercase();
        !SKIP.contains(&up.as_str())
            && !w.chars().all(|c| c.is_ascii_digit())
            && !w.contains('@')
            && !is_ref_code(w)
    });
    if let Some(s) = start {
        let mut name_words: Vec<&str> = Vec::new();
        for w in words.iter().skip(s).take(6) {
            let up = w.to_uppercase();
            if SKIP.contains(&up.as_str())
                || w.chars().all(|c| c.is_ascii_digit())
                || is_ref_code(w)
            {
                break;
            }
            name_words.push(w);
        }
        if !name_words.is_empty() {
            return normalize_vendor_name(&name_words.join(" "));
        }
    }

    String::new()
}

fn normalize_vendor_name(name: &str) -> String {
    let mut s = name.trim().to_string();
    // Remove PDF artifacts "Page N". `find_ascii_ci` (Phase 4L.2.2
    // follow-up) searches `s` itself, not a `.to_lowercase()` copy — the
    // returned offset is always both correct *and* boundary-safe in `s`,
    // closing a narrow gap the original `s.to_lowercase().find("page ")`
    // + `floor_char_boundary` version left open: a length-changing
    // Unicode lowercase mapping before the match could shift `pos` off
    // its true position in `s`, and `floor_char_boundary` alone only
    // prevented that from panicking — it didn't guarantee the resulting
    // cut was still at the real "Page " boundary.
    if let Some(pos) = find_ascii_ci(&s, "page ") {
        if pos > 0 {
            s = s[..pos].trim().to_string();
        }
    }
    // Remove trailing "L PROP", "PROPR", "PROPRIETOR"
    for suffix in &["L PROP", "PROPR", "PROPRIETOR"] {
        let lower = s.to_lowercase();
        if let Some(pos) = lower.rfind(suffix) {
            if pos > 2 {
                let pos = floor_char_boundary(&s, pos);
                s = s[..pos].trim().to_string();
            }
        }
    }
    // Remove trailing 1-2 uppercase letters (OCR noise)
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() > 1 {
        if let Some(last) = words.last() {
            if last.len() <= 2 && last.chars().all(|c| c.is_ascii_uppercase()) {
                s = words[..words.len() - 1].join(" ");
            }
        }
    }
    // Remove trailing bare account number
    let ws: Vec<&str> = s.split_whitespace().collect();
    if let Some(last) = ws.last() {
        if last.len() >= 6 && last.chars().all(|c| c.is_ascii_digit()) {
            s = ws[..ws.len() - 1].join(" ");
        }
    }
    // Collapse and cap length
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 40 {
        safe_prefix(&s, 40).trim().to_string()
    } else {
        s
    }
}

/// Smart duplicate detection — 3 passes (exact hash, ref+amount, similarity).
pub fn detect_duplicates(txns: &mut [Transaction]) {
    use std::collections::HashMap;

    // Pass 1: Exact hash duplicates
    let mut seen_hashes: HashMap<String, bool> = HashMap::new();
    for t in txns.iter_mut() {
        if t.is_opening_balance || matches!(t.status, TransactionStatus::Manual) {
            continue;
        }
        let h = t.hash();
        match seen_hashes.entry(h) {
            std::collections::hash_map::Entry::Occupied(_) => {
                t.dup_flag = true;
                if !t.tags.contains(&"DUP".to_string()) {
                    t.tags.push("DUP".to_string());
                }
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(true);
            }
        }
    }

    // Pass 2: Reference + amount match
    let mut ref_amt: HashMap<String, bool> = HashMap::new();
    for t in txns.iter_mut() {
        // Requirement #10 fix: Pass 1 above already exempts Manual (manually
        // added) rows from duplicate detection — matching the documented
        // guarantee that they're "not subject to deduplication" — but Pass 2
        // was missing the same check, so a manually added row sharing a
        // reference+amount with an imported one could still get flagged.
        if t.is_opening_balance || t.dup_flag || matches!(t.status, TransactionStatus::Manual) {
            continue;
        }
        let r = t.reference.trim().to_string();
        let amt = t.debit.or(t.credit).unwrap_or(0.0);
        if !r.is_empty() && amt > 0.0 {
            let key = format!("{}|{:.2}", r, amt);
            match ref_amt.entry(key) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    t.dup_flag = true;
                    if !t.tags.contains(&"DUP".to_string()) {
                        t.tags.push("DUP".to_string());
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(true);
                }
            }
        }
    }
}

// ── Narration token similarity (Jaccard overlap) ──────────────────────────────

fn _narr_similarity(a: &str, b: &str) -> f64 {
    let tokens = |s: &str| -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        for m in s.to_uppercase().split(|c: char| !c.is_alphanumeric()) {
            let tok = m.trim().to_string();
            if tok.len() >= 4 || (tok.len() >= 6 && tok.chars().all(|c| c.is_ascii_digit())) {
                set.insert(tok);
            }
        }
        set
    };
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let common = ta.iter().filter(|t| tb.contains(*t)).count();
    common as f64 / ta.len().max(tb.len()) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_gst_igst() {
        assert_eq!(detect_gst("IGST PAYMENT", ""), Some("GST".to_string()));
    }

    #[test]
    fn detect_gst_tax() {
        assert_eq!(
            detect_gst("ADVANCE TAX PAYMENT", ""),
            Some("TAX".to_string())
        );
    }

    #[test]
    fn extract_party_upi() {
        let name = extract_party_name("UPI/CR/12345/RAMESH KUMAR/ramesh@okaxis");
        assert!(!name.is_empty(), "should extract RAMESH KUMAR");
    }

    #[test]
    fn infer_voucher_atm() {
        let mut t = crate::parser::Transaction::new("test");
        t.narration = "ATM CASH WITHDRAWAL".to_string();
        let vt = infer_voucher_type(&t, "ATM CASH WITHDRAWAL");
        assert!(matches!(vt, VoucherType::Contra));
    }

    // ── detect_duplicates (Requirement #10) ───────────────────────────────────

    fn dup_txn(id: &str, date: &str, narration: &str, debit: Option<f64>, reference: &str) -> Transaction {
        Transaction {
            date: date.to_string(),
            narration: narration.to_string(),
            reference: reference.to_string(),
            debit,
            ..Transaction::new(id)
        }
    }

    #[test]
    fn exact_duplicate_flags_only_the_second_occurrence() {
        // Same date + narration + debit + credit (Transaction::hash()) →
        // Pass 1. The first occurrence is the "real" one; only the repeat
        // is marked, matching a bank statement that accidentally repeats a
        // row (e.g. a PDF page-overlap parsing artifact).
        let mut txns = vec![
            dup_txn("t1", "05/04/2026", "AIRTEL POSTPAID BILL", Some(499.0), ""),
            dup_txn("t2", "05/04/2026", "AIRTEL POSTPAID BILL", Some(499.0), ""),
        ];
        detect_duplicates(&mut txns);
        assert!(!txns[0].dup_flag, "first occurrence is not itself a duplicate");
        assert!(txns[1].dup_flag, "second occurrence must be flagged");
        assert!(txns[1].tags.contains(&"DUP".to_string()));
    }

    #[test]
    fn same_reference_and_amount_on_different_dates_is_flagged_by_pass_two() {
        let mut txns = vec![
            dup_txn("t1", "01/04/2026", "NEFT PAYMENT", Some(2500.0), "UTR12345"),
            dup_txn("t2", "01/05/2026", "NEFT PAYMENT RETRY", Some(2500.0), "UTR12345"),
        ];
        detect_duplicates(&mut txns);
        assert!(txns[1].dup_flag, "matching reference + amount must be flagged even with a different date/narration");
    }

    #[test]
    fn distinct_transactions_are_never_flagged() {
        let mut txns = vec![
            dup_txn("t1", "01/04/2026", "AIRTEL POSTPAID BILL", Some(499.0), "REF1"),
            dup_txn("t2", "02/04/2026", "SALARY CREDIT", None, "REF2"),
        ];
        txns[1].credit = Some(50_000.0);
        detect_duplicates(&mut txns);
        assert!(!txns[0].dup_flag);
        assert!(!txns[1].dup_flag);
    }

    #[test]
    fn manual_transaction_is_never_flagged_even_with_a_matching_reference_and_amount() {
        // PRD guarantee (section 6.12 / main.rs add-txn): manually added
        // transactions are "not subject to deduplication". Pass 1 already
        // excluded Manual rows; Pass 2 (reference+amount) previously did not
        // — this is the regression test for that fix.
        let mut manual = dup_txn("m1", "10/04/2026", "MANUAL ENTRY", Some(2500.0), "UTR12345");
        manual.status = TransactionStatus::Manual;
        let mut txns = vec![
            manual,
            dup_txn("t2", "01/05/2026", "NEFT PAYMENT", Some(2500.0), "UTR12345"),
        ];
        detect_duplicates(&mut txns);
        assert!(!txns[0].dup_flag, "the manually added row must never be flagged");
    }

    #[test]
    fn manual_transaction_does_not_cause_a_real_transaction_to_be_flagged_either() {
        // The exemption must be bidirectional: a Manual row must not "claim"
        // a reference+amount key and cause a later real import to be
        // incorrectly flagged as a duplicate of it.
        let mut manual = dup_txn("m1", "10/04/2026", "MANUAL ENTRY", Some(2500.0), "UTR12345");
        manual.status = TransactionStatus::Manual;
        let mut txns = vec![
            manual,
            dup_txn("t2", "01/05/2026", "NEFT PAYMENT", Some(2500.0), "UTR12345"),
        ];
        detect_duplicates(&mut txns);
        assert!(
            !txns[1].dup_flag,
            "a real transaction must not be flagged just because a Manual row shares its reference+amount"
        );
    }

    #[test]
    fn opening_balance_row_is_never_flagged() {
        let mut txns = vec![
            Transaction {
                is_opening_balance: true,
                balance: Some(10_000.0),
                ..Transaction::new("ob")
            },
            dup_txn("t1", "01/04/2026", "AIRTEL POSTPAID BILL", Some(499.0), ""),
        ];
        detect_duplicates(&mut txns);
        assert!(!txns[0].dup_flag);
    }

    // ── Requirement #5 (imported vs system-generated data-integrity rule) ────

    #[test]
    fn classify_one_never_mutates_the_raw_imported_fields() {
        // Classification may freely set vendor/account_head/txn_type/status/
        // confidence/tags/classification_source/gst_* — but the fields that
        // represent the actual imported bank-statement row (date, narration,
        // reference, debit, credit, balance, bank_name, account_no) must come
        // out byte-identical, or the black/imported color on those Main
        // Screen columns would be a lie.
        let mut t = crate::parser::Transaction::new("test");
        t.date = "05/04/2024".to_string();
        t.narration = "UPI/DR/239482300111/AIRTEL POSTPAID/AxisB".to_string();
        t.reference = "239482300111".to_string();
        t.debit = Some(499.0);
        t.credit = None;
        t.balance = Some(11_204.22);
        t.bank_name = "Axis Bank".to_string();
        t.account_no = "1234XXXXXX5678".to_string();

        let (date, narration, reference, debit, credit, balance, bank_name, account_no) = (
            t.date.clone(),
            t.narration.clone(),
            t.reference.clone(),
            t.debit,
            t.credit,
            t.balance,
            t.bank_name.clone(),
            t.account_no.clone(),
        );

        classify_one(&mut t, "Bank Ledger", &[], true, true);

        assert_eq!(t.date, date);
        assert_eq!(t.narration, narration);
        assert_eq!(t.reference, reference);
        assert_eq!(t.debit, debit);
        assert_eq!(t.credit, credit);
        assert_eq!(t.balance, balance);
        assert_eq!(t.bank_name, bank_name);
        assert_eq!(t.account_no, account_no);
    }

    #[test]
    fn classify_one_surfaces_gst_analysis_onto_the_transaction() {
        // Previously gst_engine::analyse()'s result was used for one ledger
        // fallback and then dropped — rate/amount/type never reached the
        // transaction at all. See PRODUCTION_READINESS_AUDIT_2026-06-22.md
        // Phase 2 item 3.
        let mut t = crate::parser::Transaction::new("test");
        t.narration = "AIRTEL POSTPAID BILL".to_string();
        t.debit = Some(999.0);
        classify_one(&mut t, "Bank Ledger", &[], true, true);
        assert_eq!(t.gst_rate, Some(18.0));
        assert!(t.gst_amount.is_some());
        assert!(t.gst_type.is_some());
        assert!(t.tags.contains(&"GST".to_string()));
    }

    #[test]
    fn classify_one_leaves_gst_fields_none_when_no_gst_signal() {
        let mut t = crate::parser::Transaction::new("test");
        t.narration = "SALARY CREDIT".to_string();
        t.credit = Some(50000.0);
        classify_one(&mut t, "Bank Ledger", &[], true, true);
        assert_eq!(t.gst_rate, None);
        assert_eq!(t.gst_amount, None);
        assert_eq!(t.gst_type, None);
    }

    // ── Settings wiring: gst_enabled / gst_auto_ledgers ───────────────────────
    //
    // "EXCITEL BILL PAYMENT" is deliberately used here instead of "AIRTEL..."
    // (used above): AIRTEL is *also* one of `kw_match`'s own telecom keywords
    // (line ~203), so it sets a non-empty `account_head` on its own, before the
    // GST block even runs — that would make the ledger-autofill assertions
    // below meaningless. EXCITEL is only in `gst_engine`'s vendor map, not in
    // `kw_match`, so `account_head` genuinely starts blank going into the GST
    // block, isolating what these tests are actually checking.

    #[test]
    fn gst_disabled_suppresses_engine_analysis_entirely() {
        let mut t = crate::parser::Transaction::new("test");
        t.narration = "EXCITEL BILL PAYMENT".to_string();
        t.debit = Some(999.0);
        classify_one(&mut t, "Bank Ledger", &[], false, true);
        assert_eq!(
            t.gst_rate, None,
            "GST engine must not run when gst_enabled=false"
        );
        assert_eq!(t.gst_amount, None);
        assert_eq!(t.gst_type, None);
        assert!(!t.tags.contains(&"GST".to_string()));
        assert!(
            t.account_head.is_empty(),
            "no ledger auto-fill from a disabled engine"
        );
    }

    #[test]
    fn gst_auto_ledgers_false_still_surfaces_gst_fields_but_skips_ledger_autofill() {
        let mut t = crate::parser::Transaction::new("test");
        t.narration = "EXCITEL BILL PAYMENT".to_string();
        t.debit = Some(999.0);
        classify_one(&mut t, "Bank Ledger", &[], true, false);
        assert_eq!(
            t.gst_rate,
            Some(18.0),
            "rate/amount/type still surface when gst_enabled=true"
        );
        assert!(t.gst_amount.is_some());
        assert!(t.gst_type.is_some());
        assert!(t.tags.contains(&"GST".to_string()));
        assert!(
            t.account_head.is_empty(),
            "ledger must not be auto-filled when gst_auto_ledgers=false"
        );
    }

    #[test]
    fn gst_auto_ledgers_true_fills_blank_account_head_from_engine_suggestion() {
        let mut t = crate::parser::Transaction::new("test");
        t.narration = "EXCITEL BILL PAYMENT".to_string();
        t.debit = Some(999.0);
        classify_one(&mut t, "Bank Ledger", &[], true, true);
        assert_eq!(t.account_head, "Internet Charges");
    }

    // ── UTF-8 crash hardening (Phase 4L.2.2) ────────────────────────────────

    /// `detect_gst`'s GSTIN scan used to byte-index every position of the
    /// (uppercased) narration+reference string looking for a 15-char GSTIN,
    /// which panicked on any narration containing a multi-byte character
    /// (₹, accented names, Devanagari, ...) at least 15 bytes long. Must not
    /// panic, and must still find a genuine ASCII GSTIN elsewhere in the
    /// same string.
    #[test]
    fn detect_gst_does_not_panic_on_multibyte_narration_and_still_finds_a_real_gstin() {
        let narration = "पेमेंट Café Münchën ₹4,999 GSTIN 27ABCDE1234F1Z5 charge";
        assert_eq!(detect_gst(narration, ""), Some("GST".to_string()));
    }

    #[test]
    fn detect_gst_does_not_panic_on_pure_multibyte_narration_with_no_gstin() {
        let narration = "पेमेंट भुगतान चालान ₹₹₹ शुल्क विवरण नारायण मूल्य श्रेणी";
        assert_eq!(detect_gst(narration, ""), None);
    }

    /// `extract_party_name`'s `is_junk` IFSC-code check used to byte-slice
    /// the first 4 bytes of an 11-*byte* token without checking it was also
    /// 11 *characters* — panicking on a multi-byte token. Must not panic on
    /// any narration shape, regardless of what (if anything) it extracts.
    #[test]
    fn extract_party_name_does_not_panic_on_multibyte_tokens() {
        let samples = [
            "मुंबई शाखा UPI/CR/12345/RAMESH KUMAR",
            "café-münchën-üö-payment-1234567",
            "₹₹₹₹₹₹₹₹₹₹₹ NEFT TRANSFER TO VENDOR",
        ];
        for s in samples {
            let _ = extract_party_name(s); // must not panic
        }
    }

    /// `normalize_vendor_name`'s final 40-byte truncation used to panic on
    /// any extracted party name longer than 40 bytes containing a
    /// multi-byte character at the cut point.
    #[test]
    fn extract_party_name_does_not_panic_when_truncating_a_long_multibyte_name() {
        let narration = "NEFT-N123456789012-राजेश कुमार शर्मा एंड संस प्राइवेट लिमिटेड-REF001";
        let name = extract_party_name(narration);
        assert!(
            name.len() <= 40,
            "must be truncated to at most 40 bytes: {name:?}"
        );
    }

    /// Phase 4L.2.2 follow-up: `normalize_vendor_name`'s "Page N" strip
    /// used to find the cut position in a `.to_lowercase()` *copy* of the
    /// name, then apply that byte offset to the *original* — silently
    /// wrong (not just unsafe) whenever a length-changing Unicode
    /// lowercase mapping (e.g. `İ` U+0130, 2 bytes → `i̇`, 3 bytes)
    /// appears before the match, shifting every position after it out of
    /// alignment. `floor_char_boundary` alone only prevented a panic —
    /// byte 8 here is already a valid boundary in the *original* string,
    /// so the old code would have silently cut mid-word ("İCafe P")
    /// instead of at the real "Page " boundary, with no panic and no
    /// warning. Must now produce the *correct* result.
    #[test]
    fn normalize_vendor_name_finds_page_marker_correctly_past_a_length_changing_unicode_char() {
        let name = normalize_vendor_name("\u{0130}Cafe Page 2 more text");
        assert_eq!(
            name, "İCafe",
            "must cut right before \"Page\", not mid-word"
        );
    }
}
