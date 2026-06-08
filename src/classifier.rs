// classifier.rs — Auto-classification engine.
// Ports App._classify(), _kwMatch(), _detectGST(), _inferVoucherType(),
// _extractPartyName(), and _detectDuplicates() from the original app.js.

use crate::parser::{Transaction, TransactionStatus, VoucherType};

// ── Public entry point ─────────────────────────────────────────────────────────

/// Run full auto-classification pipeline on all transactions in place.
/// Returns the number of transactions whose status changed.
pub fn classify_all(txns: &mut Vec<Transaction>, bank_ledger: &str) -> usize {
    let mut changed = 0;
    for t in txns.iter_mut() {
        if t.is_opening_balance { continue; }
        // Don't overwrite user-confirmed rows
        if matches!(t.status, TransactionStatus::Classified) && t.confidence >= 1.0 { continue; }
        // Don't overwrite suspense
        if matches!(t.status, TransactionStatus::Suspense) { continue; }

        let before_status = t.status.clone();
        classify_one(t, bank_ledger);
        if t.status != before_status { changed += 1; }
    }
    // Detect duplicates across the full list
    detect_duplicates(txns);
    changed
}

/// Classify a single transaction using keyword heuristics.
fn classify_one(t: &mut Transaction, bank_ledger: &str) {
    let upper = t.narration.to_uppercase();

    // 1. Keyword heuristics
    if let Some(kw) = kw_match(&upper, t, bank_ledger) {
        t.vendor       = kw.vendor;
        t.account_head = kw.head;
        t.txn_type     = kw.txn_type;
        t.confidence   = 0.45;
        t.status       = TransactionStatus::Classified;
    } else {
        // 2. Extract party name for unreviewed
        let party = extract_party_name(&t.narration);
        if !party.is_empty() {
            t.vendor = party;
        }
        t.status     = TransactionStatus::Unreviewed;
        t.confidence = 0.0;
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
}

struct KwResult {
    vendor:   String,
    head:     String,
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
    kw!("Self", "Cash", VoucherType::Contra,
        "ATM WDL","ATM-WDL","ATM CASH","ATM/","CASH WITHDRAWAL","CASH WDL","CASH DEP");

    // Bank interest earned
    kw!(bank_ledger, "Interest Income", VoucherType::Receipt,
        "INTEREST CREDITED","INT.CR","INTEREST CR","INT CR","INTEREST CREDIT",
        "FD INTEREST","FD MATURITY","RD MATURITY","INTEREST ON FD");

    // Bank interest charged / bank fees
    kw!(bank_ledger, "Bank Charges", VoucherType::Payment,
        "INTEREST CHARGED","INT.DEB","INTEREST DR","INTEREST DEBIT",
        "BANK CHARGES","SERVICE CHARGE","SMS CHARGES","ANNUAL FEE","LEDGER FOLIO",
        "CHGS RECOVERED","MIN BAL CHGS","PROCESSING FEE","ACCOUNT MAINTENANCE",
        "DEBIT CARD FEE","CREDIT CARD FEE","LOCKER CHARGES");

    // GST payment
    kw!("Government (GST)", "GST Payable", VoucherType::Payment,
        "GST PAYMENT","GST PMT","IGST PMT","CGST PMT","SGST PMT",
        "NSDL GST","GST CHALLAN");

    // Income Tax / TDS
    kw!("Income Tax Dept", "Income Tax Payable", VoucherType::Payment,
        "INCOME TAX","ADVANCE TAX","SELF ASSESSMENT TAX","TDS PAYMENT","TDS PMT",
        "CHALLAN 280","ITNS 280","TRACES TDS","TAX DEDUCTED");

    // Salary / Payroll
    kw!("Staff", "Salaries", VoucherType::Payment,
        "SALARY","SAL/","SAL-","SALARY CREDIT","PAYROLL","WAGES");

    // Rent
    kw!("Landlord", "Rent", VoucherType::Payment,
        "RENT","RENTAL","LEASE PAYMENT","RENT PAYMENT");

    // Fuel
    kw!("Fuel Station", "Fuel Expense", VoucherType::Payment,
        "PETROL","DIESEL","FUEL","BPCL","HPCL","IOCL","INDIAN OIL",
        "BHARAT PETRO","SHELL INDIA","HP PUMP","RELIANCE PETRO");

    // Telecom
    if ["AIRTEL","VODAFONE","VODAFON","VI/","JIO","BSNL","MTNL",
        "TATASKY","DISH TV","BROADBAND","JIOFIBER","TATA SKY"]
        .iter().any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("AIRTEL") { "Airtel" }
            else if upper.contains("JIO")        { "Jio" }
            else if upper.contains("BSNL")       { "BSNL" }
            else if upper.contains("VODAFONE") || upper.contains("VI/") { "Vodafone" }
            else                                 { "Telecom" };
        return Some(KwResult { vendor: vendor.to_string(), head: "Telephone Expense".to_string(), txn_type: VoucherType::Payment });
    }

    // Electricity
    kw!("Electricity Board", "Electricity Charges", VoucherType::Payment,
        "MSEDCL","BESCOM","BSES","TNEB","WBSEDCL","TORRENT POWER",
        "TPDDL","TANGEDCO","ELECTRICITY","ELECTRIC BILL","POWER BILL");

    // Insurance
    kw!("Insurance Co", "Insurance Premium", VoucherType::Payment,
        "LIC PREMIUM","LIC/","HDFC LIFE","MAX LIFE","ICICI PRU",
        "STAR HEALTH","NIVA BUPA","NEW INDIA ASSURANCE","UNITED INDIA",
        "ORIENTAL INS","INSURANCE PREMIUM","INS PREM");

    // Food delivery
    if ["SWIGGY","ZOMATO","UBER EATS","DUNZO"].iter().any(|k| upper.contains(k)) {
        let vendor = if upper.contains("SWIGGY") { "Swiggy" }
            else if upper.contains("ZOMATO")     { "Zomato" }
            else                                 { "Food Delivery" };
        return Some(KwResult { vendor: vendor.to_string(), head: "Food Expense".to_string(), txn_type: VoucherType::Payment });
    }

    // Daily food / restaurants
    kw!("Food / Dining", "Food Expense", VoucherType::Payment,
        "BREAD","BAKERY","BISCUIT","CANTEEN","TIFFIN","LUNCH",
        "DINNER","BREAKFAST","SNACKS","CHAI","TEA STALL","FOOD",
        "RESTAURANT","HOTEL DINING","MESS","DHABA","CAFE",
        "HALDIRAM","DOMINOS","PIZZA HUT","MCDONALDS","KFC",
        "SUBWAY","BURGER KING","STARBUCKS","CHAAYOS");

    // Grocery
    kw!("Grocery / Kirana", "Grocery Expense", VoucherType::Payment,
        "BIGBASKET","BIG BAZAAR","DMART","D-MART","RELIANCE FRESH",
        "RELIANCE SMART","MORE MEGASTORE","STAR BAZAAR","JIOMART",
        "GROFERS","BLINKIT","ZEPTO","INSTAMART","MILKBASKET",
        "GROCERY","KIRANA","SABZI","VEGETABLE","FRUITS","DAIRY",
        "MILK","PANEER","GHEE","OIL","RICE","WHEAT","ATTA",
        "PULSES","MASALA","SPICES","PROVISION","BHAJI");

    // Medical / Pharmacy
    kw!("Medical", "Medical Expense", VoucherType::Payment,
        "MEDPLUS","APOLLO PHARMACY","NETMEDS","TATA 1MG","1MG",
        "PHARMEASY","HEALTHKART","PRACTO","LYBRATE");

    // Online shopping
    if ["AMAZON","FLIPKART","MYNTRA","MEESHO","SNAPDEAL","NYKAA","AJIO","SHOPCLUES"]
        .iter().any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("AMAZON")   { "Amazon" }
            else if upper.contains("FLIPKART")     { "Flipkart" }
            else if upper.contains("MYNTRA")       { "Myntra" }
            else if upper.contains("MEESHO")       { "Meesho" }
            else                                   { "Online Shopping" };
        return Some(KwResult { vendor: vendor.to_string(), head: "Office Expense".to_string(), txn_type: VoucherType::Payment });
    }

    // Software / SaaS
    if ["GOOGLE","MICROSOFT","ZOOM","DROPBOX","CANVA","ADOBE","SLACK","NOTION",
        "GODADDY","BLUEHOST","HOSTINGER","GSUITE","WORKSPACE","OFFICE 365",
        "AWS","AZURE","NETLIFY","VERCEL"].iter().any(|k| upper.contains(k))
    {
        let vendor = if upper.contains("GOOGLE")    { "Google" }
            else if upper.contains("MICROSOFT")     { "Microsoft" }
            else if upper.contains("ZOOM")          { "Zoom" }
            else if upper.contains("AWS")           { "Amazon AWS" }
            else                                    { "Software" };
        return Some(KwResult { vendor: vendor.to_string(), head: "Software Expense".to_string(), txn_type: VoucherType::Payment });
    }

    // Ride-hailing
    if ["UBER","OLA CAB","OLA/","RAPIDO","MERU"].iter().any(|k| upper.contains(k)) {
        let vendor = if upper.contains("UBER") { "Uber" }
            else if upper.contains("OLA")      { "Ola" }
            else                               { "Transport" };
        return Some(KwResult { vendor: vendor.to_string(), head: "Travelling Expense".to_string(), txn_type: VoucherType::Payment });
    }

    // Travel / Hotels / Airlines
    kw!("Travel", "Travelling Expense", VoucherType::Payment,
        "IRCTC","RAILWAY","INDIGO","AIR INDIA","MAKEMYTRIP","YATRA",
        "CLEARTRIP","GOIBIBO","VISTARA","SPICEJET","AKASA");

    // Professional fees
    kw!("", "Professional Fees", VoucherType::Payment,
        "PROFESSIONAL FEES","CONSULTING","CA FEES","AUDIT FEES",
        "LEGAL FEES","ADVOCATE FEE","CHARTERED ACCOUNTANT");

    // EMI / Loan
    kw!("Bank/NBFC", "Loan Account", VoucherType::Payment,
        "EMI","LOAN EMI","HOME LOAN EMI","CAR LOAN","PERSONAL LOAN",
        "BAJAJ FINSERV","HDFC LOAN","ICICI LOAN","SBI LOAN");

    // Investments
    kw!("Investment", "Investments", VoucherType::Payment,
        "MUTUAL FUND","SIP","ZERODHA","GROWW","UPSTOX",
        "ANGEL BROKING","ICICIDIRECT","HDFC SEC","KOTAK SEC",
        "MOTILAL","NUVAMA","PAYTM MONEY");

    // Dividend
    kw!("Investment", "Dividend Income", VoucherType::Receipt,
        "DIVIDEND","DIVIDEND CREDIT");

    // Advertisement
    kw!("Advertisement", "Advertisement Expense", VoucherType::Payment,
        "GOOGLE ADS","FACEBOOK ADS","META ADS","INSTAGRAM ADS",
        "YOUTUBE ADS","DIGITAL MARKETING");

    // NEFT/RTGS/IMPS/UPI — extract party name + direction
    if ["NEFT","INFT","RTGS","IMPS","UPI","NACH","ECS","ACH","BBPS"]
        .iter().any(|k| upper.contains(k))
    {
        let party = extract_party_name(&t.narration);
        let (head, tp) = if t.credit.is_some() && t.debit.is_none() {
            (if party.is_empty() { "Sundry Debtors".to_string() } else { party.clone() }, VoucherType::Receipt)
        } else {
            (if party.is_empty() { "Sundry Creditors".to_string() } else { party.clone() }, VoucherType::Payment)
        };
        return Some(KwResult { vendor: party, head, txn_type: tp });
    }

    None
}

/// Detect GST/TAX tags from narration + reference.
pub fn detect_gst(narration: &str, reference: &str) -> Option<String> {
    let n = format!("{} {}", narration, reference).to_uppercase();
    if n.contains("IGST") || n.contains("CGST") || n.contains("SGST") {
        return Some("GST".to_string());
    }
    if n.contains("GST") && (n.contains("PAY") || n.contains("PMT") || n.contains("REFUND") || n.contains("CHALLAN")) {
        return Some("GST".to_string());
    }
    // GSTIN pattern: 15-char alphanumeric starting with 2 digits
    let bytes = n.as_bytes();
    for i in 0..bytes.len().saturating_sub(15) {
        let chunk = &n[i..i+15];
        if chunk.len() == 15
            && chunk[..2].chars().all(|c| c.is_ascii_digit())
            && chunk[2..7].chars().all(|c| c.is_ascii_alphabetic())
            && chunk[7..11].chars().all(|c| c.is_ascii_digit())
            && chunk.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Some("GST".to_string());
        }
    }
    if n.contains("INCOME TAX") || n.contains("ADVANCE TAX")
        || n.contains("TDS PMT") || n.contains("TRACES")
        || n.contains("CHALLAN 28") {
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
    if atm_re.is_match(upper) { return VoucherType::Contra; }
    if t.credit.is_some() && t.debit.is_none() { return VoucherType::Receipt; }
    if t.debit.is_some()  && t.credit.is_none() { return VoucherType::Payment; }
    VoucherType::Unknown
}

/// Extract party name from NEFT/RTGS/UPI-style narrations.
pub fn extract_party_name(narration: &str) -> String {
    const SKIP: &[&str] = &[
        "NEFT","RTGS","IMPS","UPI","INFT","NACH","ECS","ACH","BBPS",
        "CR","DR","CREDIT","DEBIT","INWARD","OUTWARD",
        "BY","TO","FROM","VIA","FOR","PER","OF","AT","ON","IN",
        "REF","UTR","TXN","NO","NUMBER","TRF",
        "TRANSFER","PAYMENT","RECEIVED","PAID","SENT",
        "P2P","P2M","P2B","P2A",
        "ONLINE","NET","BANKING","INB","MB",
        "BANK","BRANCH","IFSC",
        "CHQ","CHEQUE","DEP","DEPOSIT","WDL","WITHDRAWAL","WITH",
        "INT","INTEREST","CLG","CLEARING","CL",
        "SB","CA","OD","FD","RD","SAVINGS","CURRENT",
        "A/C","AC","ACCT","ACCOUNT",
        "AMT","AMOUNT","BAL","BALANCE",
        "CHRGS","CHGS","CHARGES","CHARGE","LEVY",
    ];
    const BANK_ABBR: &[&str] = &[
        "HDFC","HDFCBANK","ICICI","ICICIB","SBI","SBIN","AXIS","AXISB",
        "KOTAK","PNB","BOI","BOB","IOB","CANARA","UNION","IDBI","YES",
        "RBL","FEDERAL","INDUSIND","UCO","PAYTM","PHONEPE","GPAY",
    ];

    let is_junk = |s: &str| -> bool {
        let up = s.to_uppercase();
        let up = up.trim();
        if s.len() < 2 { return true; }
        if SKIP.contains(&up) { return true; }
        if s.chars().all(|c| c.is_ascii_digit()) { return true; }
        if s.contains('@') { return true; }
        // IFSC code pattern: 4 alpha + 0 + 6 alphanumeric
        if s.len() == 11 && s[..4].chars().all(|c| c.is_ascii_alphabetic()) && s.chars().nth(4) == Some('0') { return true; }
        if s.len() >= 14 && s.chars().all(|c| c.is_ascii_alphanumeric()) { return true; }
        if s.len() <= 2 && s.chars().all(|c| c.is_ascii_alphabetic()) { return true; }
        false
    };

    let is_bank_abbr = |s: &str| BANK_ABBR.contains(&s.to_uppercase().trim());

    // Case 1: delimiter-structured narration
    let parts: Vec<&str> = narration.split(['/', '-', '|', ':']).collect();
    if parts.len() > 1 {
        let mut best: Option<(&str, i32)> = None;
        for p in parts.iter().map(|s| s.trim()).filter(|s| !s.is_empty() && !is_junk(s)) {
            let words: Vec<&str> = p.split_whitespace().collect();
            let mut score = (words.len() as i32) * 3;
            if p.chars().any(|c| c.is_ascii_digit()) { score -= 2; }
            if is_bank_abbr(p) || words.first().map_or(false, |w| is_bank_abbr(w)) { score -= 6; }
            if words.len() == 1 && p.len() > 10 && p.chars().all(|c| c.is_ascii_alphanumeric()) { score -= 3; }
            if score >= 3 {
                if best.is_none() || score > best.unwrap().1 { best = Some((p, score)); }
            }
        }
        if let Some((name, _)) = best {
            return normalize_vendor_name(name);
        }
    }

    // Case 2: sentence-style — skip junk words and take first run of valid words
    let words: Vec<&str> = narration.trim().split_whitespace().collect();
    let is_ref_code = |w: &str| w.len() >= 9 && w.chars().any(|c| c.is_ascii_digit()) && w.chars().all(|c| c.is_ascii_alphanumeric());
    let start = words.iter().position(|w| {
        let up = w.to_uppercase();
        !SKIP.contains(&up.as_str()) && !w.chars().all(|c| c.is_ascii_digit()) && !w.contains('@') && !is_ref_code(w)
    });
    if let Some(s) = start {
        let mut name_words: Vec<&str> = Vec::new();
        for w in words.iter().skip(s).take(6) {
            let up = w.to_uppercase();
            if SKIP.contains(&up.as_str()) || w.chars().all(|c| c.is_ascii_digit()) || is_ref_code(w) { break; }
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
    // Remove PDF artifacts "Page N"
    if let Some(pos) = s.to_lowercase().find("page ") {
        if pos > 0 { s = s[..pos].trim().to_string(); }
    }
    // Remove trailing "L PROP", "PROPR", "PROPRIETOR"
    for suffix in &["L PROP","PROPR","PROPRIETOR"] {
        let lower = s.to_lowercase();
        if let Some(pos) = lower.rfind(suffix) {
            if pos > 2 { s = s[..pos].trim().to_string(); }
        }
    }
    // Remove trailing 1-2 uppercase letters (OCR noise)
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() > 1 {
        if let Some(last) = words.last() {
            if last.len() <= 2 && last.chars().all(|c| c.is_ascii_uppercase()) {
                s = words[..words.len()-1].join(" ");
            }
        }
    }
    // Remove trailing bare account number
    let ws: Vec<&str> = s.split_whitespace().collect();
    if let Some(last) = ws.last() {
        if last.len() >= 6 && last.chars().all(|c| c.is_ascii_digit()) {
            s = ws[..ws.len()-1].join(" ");
        }
    }
    // Collapse and cap length
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 40 { s[..40].trim().to_string() } else { s }
}

/// Smart duplicate detection — 3 passes (exact hash, ref+amount, similarity).
pub fn detect_duplicates(txns: &mut Vec<Transaction>) {
    use std::collections::HashMap;

    // Pass 1: Exact hash duplicates
    let mut seen_hashes: HashMap<String, bool> = HashMap::new();
    for t in txns.iter_mut() {
        if t.is_opening_balance || matches!(t.status, TransactionStatus::Manual) { continue; }
        let h = t.hash();
        if seen_hashes.contains_key(&h) {
            t.dup_flag = true;
            if !t.tags.contains(&"DUP".to_string()) { t.tags.push("DUP".to_string()); }
        } else {
            seen_hashes.insert(h, true);
        }
    }

    // Pass 2: Reference + amount match
    let mut ref_amt: HashMap<String, bool> = HashMap::new();
    for t in txns.iter_mut() {
        if t.is_opening_balance || t.dup_flag { continue; }
        let r = t.reference.trim().to_string();
        let amt = t.debit.or(t.credit).unwrap_or(0.0);
        if !r.is_empty() && amt > 0.0 {
            let key = format!("{}|{:.2}", r, amt);
            if ref_amt.contains_key(&key) {
                t.dup_flag = true;
                if !t.tags.contains(&"DUP".to_string()) { t.tags.push("DUP".to_string()); }
            } else {
                ref_amt.insert(key, true);
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
    if ta.is_empty() || tb.is_empty() { return 0.0; }
    let common = ta.iter().filter(|t| tb.contains(*t)).count();
    common as f64 / ta.len().max(tb.len()) as f64
}

// ── Tally group heuristic (maps account head names to Tally groups) ───────────

pub fn ledger_group(name: &str) -> &'static str {
    let n = name.to_lowercase();
    if n.contains("salary") || n.contains("wages") || n.contains("payroll") { return "Indirect Expenses"; }
    if n.contains("rent") || n.contains("lease") { return "Indirect Expenses"; }
    if n.contains("telephone") || n.contains("internet") || n.contains("broadband") || n.contains("mobile") { return "Indirect Expenses"; }
    if n.contains("electricity") || n.contains("power") || n.contains("energy") { return "Indirect Expenses"; }
    if n.contains("fuel") || n.contains("petrol") || n.contains("diesel") { return "Indirect Expenses"; }
    if n.contains("food") || n.contains("canteen") || n.contains("meal") || n.contains("restaurant") { return "Indirect Expenses"; }
    if n.contains("medical") || n.contains("medicine") || n.contains("hospital") { return "Indirect Expenses"; }
    if n.contains("grocery") { return "Indirect Expenses"; }
    if n.contains("software") || n.contains("saas") || n.contains("subscription") { return "Indirect Expenses"; }
    if n.contains("insurance") { return "Indirect Expenses"; }
    if n.contains("professional") || n.contains("consulting") || n.contains("audit") { return "Indirect Expenses"; }
    if n.contains("bank charge") || n.contains("service charge") { return "Indirect Expenses"; }
    if n.contains("travel") || n.contains("transport") || n.contains("travelling") { return "Indirect Expenses"; }
    if n.contains("advertisement") || n.contains("marketing") { return "Indirect Expenses"; }
    if n.contains("repair") || n.contains("maintenance") { return "Indirect Expenses"; }
    if n.contains("printing") || n.contains("stationery") { return "Indirect Expenses"; }
    if n.contains("interest income") { return "Indirect Income"; }
    if n.contains("dividend") { return "Indirect Income"; }
    if n.contains("commission") { return "Indirect Income"; }
    if n.contains("rental income") { return "Indirect Income"; }
    if n.contains("sales") || n.contains("revenue") { return "Sales Accounts"; }
    if n.contains("purchase") { return "Purchase Accounts"; }
    if n.contains("gst") || n.contains("igst") || n.contains("cgst") || n.contains("sgst") { return "Duties & Taxes"; }
    if n.contains("tds") || n.contains("income tax") || n.contains("advance tax") { return "Duties & Taxes"; }
    if n.contains("creditor") { return "Sundry Creditors"; }
    if n.contains("debtor") { return "Sundry Debtors"; }
    if n.contains("cash") { return "Cash-in-Hand"; }
    if n.contains("loan") || n.contains("borrowing") { return "Loans (Liability)"; }
    if n.contains("capital") { return "Capital Account"; }
    if n.contains("investment") || n.contains("mutual fund") { return "Investments"; }
    "Indirect Expenses"
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
        assert_eq!(detect_gst("ADVANCE TAX PAYMENT", ""), Some("TAX".to_string()));
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
}
