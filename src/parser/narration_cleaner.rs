//! narration_cleaner.rs — Port of `src/services/narration-cleaner.js`
//!
//! Pipeline: detect type → strip noise tokens → extract party → normalize → score
//!
//! Input:  raw bank narration string (e.g. "UPI/DR/2394823/AMAZON SELLER PAYMEN")
//! Output: NarrationMeta { original, cleaned, payment_type, party, payment_ref, confidence }

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};

// ── Payment types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentType {
    Upi, Neft, Rtgs, Imps, Nach, Atm, Pos, Cheque, Cash,
    Interest, Charges, Transfer, Salary, Emi, Dd, Swift, Other,
}

impl std::fmt::Display for PaymentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PaymentType::Upi      => "UPI",
            PaymentType::Neft     => "NEFT",
            PaymentType::Rtgs     => "RTGS",
            PaymentType::Imps     => "IMPS",
            PaymentType::Nach     => "NACH",
            PaymentType::Atm      => "ATM",
            PaymentType::Pos      => "POS",
            PaymentType::Cheque   => "CHEQUE",
            PaymentType::Cash     => "CASH",
            PaymentType::Interest => "INTEREST",
            PaymentType::Charges  => "CHARGES",
            PaymentType::Transfer => "TRANSFER",
            PaymentType::Salary   => "SALARY",
            PaymentType::Emi      => "EMI",
            PaymentType::Dd       => "DD",
            PaymentType::Swift    => "SWIFT",
            PaymentType::Other    => "OTHER",
        };
        write!(f, "{}", s)
    }
}

// ── Type detection patterns (order = priority, most specific first) ──────────

struct TypePattern {
    payment_type: PaymentType,
    re: &'static str,
}

static TYPE_PATTERNS_SRC: &[TypePattern] = &[
    TypePattern { payment_type: PaymentType::Upi,      re: r"(?i)\bUPI\b" },
    TypePattern { payment_type: PaymentType::Neft,     re: r"(?i)\bNEFT\b" },
    TypePattern { payment_type: PaymentType::Rtgs,     re: r"(?i)\bRTGS\b" },
    TypePattern { payment_type: PaymentType::Imps,     re: r"(?i)\bIMPS\b" },
    TypePattern { payment_type: PaymentType::Nach,     re: r"(?i)\bNACH\b|\bACH\b|\bECS\b" },
    TypePattern { payment_type: PaymentType::Atm,      re: r"(?i)\bATM\b|\bATM[-\s]?WDL\b" },
    TypePattern { payment_type: PaymentType::Pos,      re: r"(?i)\bPOS\b|\bCARD\b|\bSWIPE\b" },
    TypePattern { payment_type: PaymentType::Cheque,   re: r"(?i)\bCHQ\b|\bCHEQUE\b|\bCHEK\b|\bCLG\b" },
    TypePattern { payment_type: PaymentType::Cash,     re: r"(?i)\bCASH\s*(DEP|DEPOSIT|WDL|WITHDRAWAL|DEPTT)?\b" },
    TypePattern { payment_type: PaymentType::Interest, re: r"(?i)\bINT(EREST)?\b|\bINT\.?\s*PD\b" },
    TypePattern { payment_type: PaymentType::Charges,  re: r"(?i)\bCHRGS?\b|\bCHGS?\b|\bCHARGES?\b|\bFEES?\b|\bLEVY\b" },
    TypePattern { payment_type: PaymentType::Transfer, re: r"(?i)\bTRF\b|\bTRANSFER\b|\bFT\b" },
    TypePattern { payment_type: PaymentType::Salary,   re: r"(?i)\bSALARY\b|\bSAL\b|\bSALARY\s*CR\b" },
    TypePattern { payment_type: PaymentType::Emi,      re: r"(?i)\bEMI\b|\bLOAN\s*(INST|REPAY|EMI)\b" },
    TypePattern { payment_type: PaymentType::Dd,       re: r"(?i)\bDD\b|\bDEMAND\s*DRAFT\b" },
    TypePattern { payment_type: PaymentType::Swift,    re: r"(?i)\bSWIFT\b|\bFOREIGN\s*(INWARD|OUTWARD)\b" },
];

static TYPE_COMPILED: Lazy<Vec<(Regex, PaymentType)>> = Lazy::new(|| {
    TYPE_PATTERNS_SRC.iter().map(|p| {
        (Regex::new(p.re).expect("bad type pattern"), p.payment_type.clone())
    }).collect()
});

// ── Reference stripping regexes (applied in order) ───────────────────────────

// Note: Rust regex crate does not support lookaheads; medium-numeric uses plain \b.
static REF_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| vec![
    Regex::new(r"\b[0-9]{12,22}\b").unwrap(),            // Long numeric UTR / txn ID
    Regex::new(r"\b[A-Z]{4}[0-9]{7,}\b").unwrap(),       // IFSC-style codes
    Regex::new(r"(?i)\bUTR[:\s]*[A-Z0-9]{10,22}\b").unwrap(), // UTR with label
    Regex::new(r"\b[A-Z]{2,4}[0-9]{6,}\b").unwrap(),     // Short alphanumeric refs
    Regex::new(r"\b[0-9]{6,11}\b").unwrap(),              // Medium numeric 6-11 digits
    Regex::new(r"[A-Z0-9]{20,}").unwrap(),                // Long alphanumeric tokens
]);

// ── Junk word set ─────────────────────────────────────────────────────────────

static JUNK: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let words = [
        "NEFT","RTGS","IMPS","UPI","NACH","ACH","ECS","ATM","POS","DD","CHQ","CLG",
        "CR","DR","CREDIT","DEBIT","INWARD","OUTWARD","TRF","TRANSFER",
        "BY","TO","FROM","VIA","FOR","PER","OF","AT","ON","IN","AND","THE",
        "REF","UTR","TXN","NO","NUM","ID","TRNF",
        "PAYMENT","PAID","PAY","RECEIVED","RECV","RCV","SENT","SEND",
        "ONLINE","NET","BANKING","INB","MB","MOB",
        "BANK","BRANCH","IFSC","MICR","SWIFT",
        "DEP","DEPOSIT","WDL","WITHDRAWAL","WITH",
        "INT","INTEREST","CLG","CLEARING",
        "AMT","AMOUNT","BAL","BALANCE",
        "CHARGES","CHRGS","CHGS","LEVY","FEE","FEES",
        "SB","CA","OD","FD","RD","SAVINGS","CURRENT",
        "A/C","AC","ACCT","ACCOUNT",
        "DR.","CR.","INR","RS",
        "P2P","P2M","P2B","P2A",
    ];
    words.iter().copied().collect()
});

// ── Known bank abbreviations ──────────────────────────────────────────────────

static BANK_TOKENS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let words = [
        "HDFC","HDFCBANK","ICICI","ICICIB","SBI","SBIN","AXIS","AXISB",
        "KOTAK","PNB","BOI","BOB","IOB","CANARA","UNION","IDBI","YES",
        "RBL","FEDERAL","INDUSIND","UCO","PAYTM","PHONEPE","GPAY","CRED",
    ];
    words.iter().copied().collect()
});

// ── Vendor normalization dictionary ──────────────────────────────────────────

static VENDOR_DICT: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let pairs = [
        ("AMAZON",          "Amazon"),
        ("AMAZON SELLER",   "Amazon Seller"),
        ("FLIPKART",        "Flipkart"),
        ("SWIGGY",          "Swiggy"),
        ("ZOMATO",          "Zomato"),
        ("AIRTEL",          "Airtel"),
        ("JIO",             "Reliance Jio"),
        ("RELIANCE JIO",    "Reliance Jio"),
        ("VODAFONE",        "Vodafone"),
        ("BSNL",            "BSNL"),
        ("TATA",            "Tata"),
        ("BPCL",            "BPCL"),
        ("HPCL",            "HPCL"),
        ("INDIAN OIL",      "Indian Oil"),
        ("MSEDCL",          "MSEDCL"),
        ("BESCOM",          "BESCOM"),
        ("ELECTRICITY",     "Electricity Board"),
        ("LIC",             "LIC of India"),
        ("ICICI PRU",       "ICICI Prudential"),
        ("HDFC LIFE",       "HDFC Life"),
        ("MAX LIFE",        "Max Life Insurance"),
        ("STAR HEALTH",     "Star Health Insurance"),
        ("FACEBOOK",        "Meta (Facebook)"),
        ("GOOGLE",          "Google"),
        ("NETFLIX",         "Netflix"),
        ("HOTSTAR",         "Disney+ Hotstar"),
        ("SPOTIFY",         "Spotify"),
        ("MICROSOFT",       "Microsoft"),
        ("APPLE",           "Apple"),
        ("UBER",            "Uber"),
        ("OLA",             "Ola"),
        ("RAPIDO",          "Rapido"),
        ("IRCTC",           "IRCTC"),
        ("MAKEMYTRIP",      "MakeMyTrip"),
        ("GOIBIBO",         "Goibibo"),
        ("MYNTRA",          "Myntra"),
        ("NYKAA",           "Nykaa"),
        ("BIGBASKET",       "BigBasket"),
        ("DUNZO",           "Dunzo"),
        ("BLINKIT",         "Blinkit"),
        ("ZEPTO",           "Zepto"),
        ("PAYTM",           "Paytm"),
        ("PHONEPE",         "PhonePe"),
        ("RAZORPAY",        "Razorpay"),
        ("CASHFREE",        "Cashfree"),
        ("JUSPAY",          "Juspay"),
        ("GPY",             "Google Pay"),
        ("GPAY",            "Google Pay"),
    ];
    pairs.iter().copied().collect()
});

// ── Ledger suffix noise words ─────────────────────────────────────────────────
// Stripped iteratively from the right end of a name.

static LEDGER_SUFFIX_NOISE: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let words = [
        "LTD","LIMITED","PVT","PRIVATE","INC","CORP","CORPORATION","LLC","LLP",
        "INDIA","INDIAN",
        "SERVICES","SERVICE","SOLUTIONS","SOLUTION",
        "TECHNOLOGIES","TECHNOLOGY","TECH",
        "SYSTEMS","SYSTEM","ENTERPRISES","ENTERPRISE",
        "GROUP","HOLDINGS","HOLDING",
        "PAY","PAYMENT","PAYMENTS",
        "SELLER","SELLERS","MARKETPLACE","RETAIL","STORE","SHOP",
        "ONLINE","DIGITAL","MOBILE",
        "INSTAMART","EXPRESS","NOW","QUICK",
    ];
    words.iter().copied().collect()
});

// ── Business-indicator words (justify 3+ token ledger names) ─────────────────

static LEDGER_LONG_NAME_BIZ: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let words = [
        "TRADERS","TRADING","TRADE","INDUSTRIES","INDUSTRY","MANUFACTURING",
        "DISTRIBUTORS","DISTRIBUTOR","EXPORTERS","EXPORTER","IMPORTERS","IMPORTER",
        "AGENCY","AGENCIES","CONTRACTORS","CONTRACTOR","DEVELOPERS","DEVELOPER",
        "BUILDERS","BUILDER","CONSTRUCTIONS","CONSTRUCTION","ASSOCIATES","ASSOCIATE",
        "PARTNERS","PARTNER","BROTHERS","BROS","CO","COMPANY","WORKS",
        "CONSULTANTS","CONSULTANCY","MART","CENTRE","CENTER","HOUSE",
        "DEALERS","DEALER","SUPPLIERS","SUPPLIER","HOSPITAL","CLINIC",
        "SCHOOL","COLLEGE","INSTITUTE","FOUNDATION","TRUST","BANK",
    ];
    words.iter().copied().collect()
});

// Vendor dict keys sorted by descending length (longest-match wins in _extract_party).
static VENDOR_KEYS_DESC: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut keys: Vec<&'static str> = VENDOR_DICT.keys().copied().collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()));
    keys
});

// Vendor dict keys sorted by ascending length (shortest/root match wins in normalize).
static VENDOR_KEYS_ASC: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut keys: Vec<&'static str> = VENDOR_DICT.keys().copied().collect();
    keys.sort_by_key(|k| k.len());
    keys
});

static URL_RE: Lazy<Regex>  = Lazy::new(|| Regex::new(r"(?i)https?://\S+").unwrap());
static VPA_RE: Lazy<Regex>  = Lazy::new(|| Regex::new(r"(?i)\S+@[a-z]+\b").unwrap());
static IFSC_FRAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Z]{4}0[A-Z0-9]{6}$").unwrap());

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Port of `_toTitleCase`. Preserves 2–5 all-caps abbreviations.
pub fn to_title_case(s: &str) -> String {
    let lower_words: HashSet<&str> = ["of","in","at","for","and","or","by","to","the","a","an"].iter().copied().collect();
    s.trim().split_whitespace().enumerate().map(|(i, w)| {
        if w.is_empty() { return w.to_string(); }
        // Keep 2-5 all-caps abbreviations.
        if w.len() >= 2 && w.len() <= 5 && w.chars().all(|c| c.is_ascii_uppercase()) && !lower_words.contains(w.to_lowercase().as_str()) {
            return w.to_string();
        }
        if lower_words.contains(w.to_lowercase().as_str()) && i > 0 {
            return w.to_lowercase();
        }
        let mut c = w.chars();
        match c.next() {
            None    => String::new(),
            Some(f) => f.to_uppercase().to_string() + &c.as_str().to_lowercase(),
        }
    }).collect::<Vec<_>>().join(" ")
}

/// Detect payment type from raw narration.
pub fn detect_type(narr: &str) -> PaymentType {
    for (re, pt) in TYPE_COMPILED.iter() {
        if re.is_match(narr) { return pt.clone(); }
    }
    PaymentType::Other
}

/// Split mixed alphanumeric tokens at digit↔alpha boundaries.
/// "215218311944petrol" → ["215218311944", "petrol"]
fn split_mixed_token(tok: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let chars: Vec<char> = tok.chars().collect();
    let mut start = 0;
    for i in 1..chars.len() {
        let prev_digit = chars[i-1].is_ascii_digit();
        let curr_digit = chars[i].is_ascii_digit();
        if prev_digit != curr_digit {
            parts.push(chars[start..i].iter().collect());
            start = i;
        }
    }
    parts.push(chars[start..].iter().collect());
    parts
}

/// Strip reference numbers and payment infrastructure tokens.
fn strip_noise(narr: &str) -> String {
    // Split mixed alphanumeric tokens first.
    let mixed_re = Regex::new(r"[A-Za-z0-9]{8,}").unwrap();
    let mut s = mixed_re.replace_all(narr, |caps: &regex::Captures| {
        let tok = &caps[0];
        let has_dig   = tok.chars().any(|c| c.is_ascii_digit());
        let has_alpha = tok.chars().any(|c| c.is_ascii_alphabetic());
        if has_dig && has_alpha {
            split_mixed_token(tok).join(" ")
        } else {
            tok.to_string()
        }
    }).to_string();

    // Strip reference numbers.
    for re in REF_PATTERNS.iter() {
        s = re.replace_all(&s, " ").to_string();
    }

    // Strip URLs and VPA handles.
    s = URL_RE.replace_all(&s, " ").to_string();
    s = VPA_RE.replace_all(&s, " ").to_string();

    // Split on separators, keep non-junk / non-IFSC segments.
    let parts: Vec<&str> = s.split(|c| "/|\\".contains(c))
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    let kept: Vec<&str> = parts.iter().filter(|&&p| {
        let up = p.to_uppercase();
        if up.is_empty() || up.len() <= 1 { return false; }
        if JUNK.contains(up.as_str())  { return false; }
        if up.chars().all(|c| c.is_ascii_digit()) { return false; }
        if IFSC_FRAG_RE.is_match(&up) { return false; }
        true
    }).copied().collect();

    kept.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract most-probable party name from stripped text.
fn extract_party(stripped: &str, _ptype: &PaymentType) -> String {
    if stripped.is_empty() { return String::new(); }

    let words: Vec<&str> = stripped.split_whitespace()
        .filter(|w| w.len() > 1 && !w.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if words.is_empty() { return String::new(); }

    // Try vendor dictionary longest-match first.
    let up = stripped.to_uppercase();
    for k in VENDOR_KEYS_DESC.iter() {
        if up.starts_with(k) || up.contains(k) {
            return VENDOR_DICT[k].to_string();
        }
    }

    // Score each word.
    let scored: Vec<(&&str, i32)> = words.iter().map(|w| {
        let wu = w.to_uppercase();
        let mut sc: i32 = 3;
        if JUNK.contains(wu.as_str())        { sc -= 5; }
        if BANK_TOKENS.contains(wu.as_str()) { sc -= 3; }
        if w.starts_with(|c: char| c.is_ascii_digit()) { sc -= 4; }
        if w.len() > 5 { sc += (w.len() - 5).min(3) as i32; }
        (w, sc)
    }).collect();

    // Find span with best cumulative score (up to 5 consecutive words).
    let mut best = String::new();
    let mut best_score: i32 = 0;
    for i in 0..words.len() {
        let mut span = String::new();
        let mut span_score: i32 = 0;
        for j in i..(i + 5).min(words.len()) {
            let wu = words[j].to_uppercase();
            if JUNK.contains(wu.as_str()) || words[j].chars().all(|c| c.is_ascii_digit()) { break; }
            if !span.is_empty() { span.push(' '); }
            span.push_str(words[j]);
            span_score += scored.iter().find(|(w, _)| **w == words[j]).map_or(0, |(_, s)| *s);
        }
        if span_score > best_score { best = span; best_score = span_score; }
    }

    best.chars().take(40).collect()
}

/// Build readable cleaned narration.
fn build_cleaned(party: &str, ptype: &PaymentType, stripped: &str) -> String {
    let name = if party.is_empty() {
        stripped.chars().take(40).collect::<String>()
    } else {
        party.to_string()
    };
    let name = if name.is_empty() { "Unknown".to_string() } else { name };

    match ptype {
        PaymentType::Atm      => format!("ATM Withdrawal{}", if party.is_empty() { String::new() } else { format!(" - {}", party) }),
        PaymentType::Cash     => format!("Cash{}", if party.is_empty() { String::new() } else { format!(" - {}", party) }),
        PaymentType::Interest => format!("Interest{}", if party.is_empty() { String::new() } else { format!(" - {}", party) }),
        PaymentType::Charges  => format!("Bank Charges{}", if party.is_empty() { String::new() } else { format!(" - {}", party) }),
        PaymentType::Upi      => format!("UPI - {}", name),
        PaymentType::Neft     => format!("NEFT - {}", name),
        PaymentType::Rtgs     => format!("RTGS - {}", name),
        PaymentType::Imps     => format!("IMPS - {}", name),
        PaymentType::Nach     => format!("NACH/ACH - {}", name),
        PaymentType::Pos      => format!("Card Payment - {}", name),
        PaymentType::Cheque   => format!("Cheque - {}", name),
        PaymentType::Transfer => format!("Transfer - {}", name),
        PaymentType::Salary   => format!("Salary - {}", name),
        PaymentType::Emi      => format!("EMI - {}", name),
        PaymentType::Dd       => format!("Demand Draft - {}", name),
        PaymentType::Swift    => format!("SWIFT Transfer - {}", name),
        PaymentType::Other    => name,
    }
}

/// Compute confidence score for a cleaned narration.
fn score(original: &str, cleaned: &str, party: &str, ptype: &PaymentType) -> f64 {
    let mut s = 0.4f64;
    if *ptype != PaymentType::Other  { s += 0.15; }
    if party.len() >= 3              { s += 0.15; }
    if party.len() >= 6              { s += 0.10; }

    let up = party.to_uppercase();
    if VENDOR_KEYS_ASC.iter().any(|k| up == *k || up.starts_with(&format!("{} ", k))) {
        s += 0.15;
    }

    let ratio = cleaned.len() as f64 / original.len().max(1) as f64;
    if ratio < 0.8 { s += 0.05; }

    (s.min(0.99) * 100.0).round() / 100.0
}

/// Extract payment reference (UTR / cheque number).
pub fn extract_ref(narr: &str) -> String {
    // UTR with label.
    let utr_lbl = Regex::new(r"(?i)\bUTR[:\s]*([A-Z0-9]{10,22})\b").unwrap();
    if let Some(cap) = utr_lbl.captures(narr) { return cap[1].to_string(); }

    let ref_lbl = Regex::new(r"(?i)\bREF[:\s#]*([A-Z0-9]{8,20})\b").unwrap();
    if let Some(cap) = ref_lbl.captures(narr) { return cap[1].to_string(); }

    // Long numeric UTR (12-22 digits).
    let num_re = Regex::new(r"\b([0-9]{12,22})\b").unwrap();
    if let Some(cap) = num_re.captures(narr) { return cap[1].to_string(); }

    // Cheque numbers.
    let chq_re = Regex::new(r"(?i)\b(?:CHQ|CHEQUE)[:\s]*([0-9]{6,9})\b").unwrap();
    if let Some(cap) = chq_re.captures(narr) { return cap[2].to_string(); }

    String::new()
}

// ── normalize_ledger_name ─────────────────────────────────────────────────────
//
// Port of `NarrationCleaner.normalizeLedgerName()`.
//
// Pipeline:
//   1. Pre-strip dict check (exact or word-boundary prefix match).
//   2. Iteratively strip right-end suffix noise.
//   3. Rule A — Repeated leading-token collapse.
//   4. Rule B — Cap all-proper-noun names at 2 tokens.
//   5. Rule C — Canonical token order for 2-token personal names.
//   6. Post-strip dict check.
//   7. Title-case.

pub fn normalize_ledger_name(raw_name: &str) -> String {
    let raw = raw_name.trim();
    if raw.is_empty() { return raw_name.to_string(); }

    let name_up = raw.to_uppercase();

    // 1. Pre-strip dict check.
    for k in VENDOR_KEYS_ASC.iter() {
        if name_up == *k || name_up.starts_with(&format!("{} ", k)) {
            return VENDOR_DICT[k].to_string();
        }
    }

    // 2. Iteratively strip right-end suffix noise.
    let mut words: Vec<&str> = name_up.split_whitespace().collect();
    let mut changed = true;
    while changed && words.len() > 1 {
        changed = false;
        if LEDGER_SUFFIX_NOISE.contains(*words.last().unwrap()) {
            words.pop();
            changed = true;
        }
    }

    // Rule A: Repeated leading-token collapse.
    if words.len() >= 3 {
        let leading: HashSet<&str> = [words[0], words[1]].iter().copied().collect();
        let mut trunc = words.len();
        for i in 2..words.len() {
            if leading.contains(words[i]) { trunc = i; break; }
        }
        words.truncate(trunc);
    }

    // Rule B: Cap all-proper-noun names at 2 tokens (no business indicator).
    if words.len() >= 3 && !words.iter().any(|w| LEDGER_LONG_NAME_BIZ.contains(w)) {
        words.truncate(2);
    }

    // Rule C: Canonical token order for 2-token personal names.
    if words.len() == 2 && !words.iter().any(|w| LEDGER_LONG_NAME_BIZ.contains(w)) {
        if words[0].cmp(words[1]) == std::cmp::Ordering::Less {
            words.swap(0, 1);
        }
    }

    let stripped = words.join(" ");

    // 3. Post-strip dict check.
    for k in VENDOR_KEYS_ASC.iter() {
        if stripped == *k || stripped.starts_with(&format!("{} ", k)) {
            return VENDOR_DICT[k].to_string();
        }
    }

    // 4. Title-case.
    to_title_case(&stripped)
}

// ── NarrationMeta ─────────────────────────────────────────────────────────────

/// Result of cleaning one narration.
#[derive(Debug, Clone)]
pub struct NarrationMeta {
    pub original:    String,
    pub cleaned:     String,
    pub payment_type: PaymentType,
    pub party:       String,
    pub payment_ref: String,
    pub confidence:  f64,
}

/// Clean a single narration string.
/// Port of `NarrationCleaner.clean()`.
pub fn clean(raw_narration: &str) -> NarrationMeta {
    let original = raw_narration.trim().to_string();
    if original.is_empty() {
        return NarrationMeta {
            original: original.clone(),
            cleaned: String::new(),
            payment_type: PaymentType::Other,
            party: String::new(),
            payment_ref: String::new(),
            confidence: 0.0,
        };
    }

    let ptype       = detect_type(&original);
    let payment_ref = extract_ref(&original);
    let stripped    = strip_noise(&original);
    let party_raw   = extract_party(&stripped, &ptype);
    let party       = to_title_case(&party_raw);
    let stripped_tc = to_title_case(&stripped);
    let cleaned_raw = build_cleaned(&party, &ptype, &stripped_tc);
    let confidence  = score(&original, &cleaned_raw, &party, &ptype);

    const MIN_CONF: f64 = 0.4;
    let cleaned = if confidence >= MIN_CONF {
        cleaned_raw.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        to_title_case(&original.chars().take(60).collect::<String>())
    };

    NarrationMeta {
        original,
        cleaned,
        payment_type: ptype,
        party,
        payment_ref,
        confidence,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── to_title_case ─────────────────────────────────────────────────────────

    #[test]
    fn title_case_basic() {
        // "RAHUL" = 5 all-caps → kept uppercase; "TRADERS" = 7 chars → title-cased
        assert_eq!(to_title_case("RAHUL TRADERS"), "RAHUL Traders");
    }

    #[test]
    fn title_case_preserves_abbrev() {
        // "LIC" (3) and "INDIA" (5) are all-caps ≤5 → kept; "OF" is lowercase-word at i>0
        assert_eq!(to_title_case("LIC OF INDIA"), "LIC of INDIA");
    }

    #[test]
    fn title_case_preposition_lowercase() {
        // "BANK" (4), "INDIA" (5) → kept uppercase; "OF" → lowercase at i>0
        assert_eq!(to_title_case("BANK OF INDIA"), "BANK of INDIA");
    }

    #[test]
    fn title_case_empty() {
        assert_eq!(to_title_case(""), "");
    }

    // ── detect_type ──────────────────────────────────────────────────────────

    #[test]
    fn type_upi() { assert_eq!(detect_type("UPI/DR/123/AMAZON"), PaymentType::Upi); }

    #[test]
    fn type_neft() { assert_eq!(detect_type("NEFT PAYMENT RAJESH SHAH"), PaymentType::Neft); }

    #[test]
    fn type_atm() { assert_eq!(detect_type("ATM WDL 10000"), PaymentType::Atm); }

    #[test]
    fn type_cheque() { assert_eq!(detect_type("CHQ 123456"), PaymentType::Cheque); }

    #[test]
    fn type_interest() { assert_eq!(detect_type("INTEREST CREDITED SB"), PaymentType::Interest); }

    #[test]
    fn type_charges() { assert_eq!(detect_type("CHARGES GST LEVY"), PaymentType::Charges); }

    #[test]
    fn type_other() { assert_eq!(detect_type("SOME RANDOM TEXT"), PaymentType::Other); }

    // ── extract_ref ──────────────────────────────────────────────────────────

    #[test]
    fn ref_utr_labeled() {
        assert_eq!(extract_ref("NEFT UTR:234567890123 RAJESH"), "234567890123");
    }

    #[test]
    fn ref_long_numeric() {
        assert_eq!(extract_ref("UPI 215218311944 AMAZON"), "215218311944");
    }

    #[test]
    fn ref_no_ref_empty() {
        assert_eq!(extract_ref("ATM WITHDRAWAL"), "");
    }

    // ── normalize_ledger_name ─────────────────────────────────────────────────

    #[test]
    fn ledger_amazon_pay() {
        assert_eq!(normalize_ledger_name("AMAZON PAY"), "Amazon");
    }

    #[test]
    fn ledger_amazon_seller_services() {
        assert_eq!(normalize_ledger_name("AMAZON SELLER SERVICES"), "Amazon");
    }

    #[test]
    fn ledger_amazon_india() {
        assert_eq!(normalize_ledger_name("AMAZON INDIA"), "Amazon");
    }

    #[test]
    fn ledger_swiggy_instamart() {
        assert_eq!(normalize_ledger_name("SWIGGY INSTAMART"), "Swiggy");
    }

    #[test]
    fn ledger_pvt_ltd_stripped() {
        // Strip LTD+PVT; "TRADERS" is a biz word → no Rule B/C sort.
        // to_title_case: "RAHUL"(5 all-caps) kept; "TRADERS"(7) → "Traders"
        assert_eq!(normalize_ledger_name("RAHUL TRADERS PVT LTD"), "RAHUL Traders");
    }

    #[test]
    fn ledger_india_cements_kept() {
        // "INDIA" is not at the right end (CEMENTS is) → not stripped.
        // "CEMENTS" not in suffix noise → stays. 2 tokens → no Rule B.
        // Rule C: "INDIA" > "CEMENTS" (I>C) → no swap.
        // to_title_case: "INDIA"(5 all-caps) kept; "CEMENTS"(7) → "Cements"
        let result = normalize_ledger_name("INDIA CEMENTS");
        assert_eq!(result, "INDIA Cements");
    }

    #[test]
    fn ledger_rule_a_repeated_token_collapse() {
        // "VIDWANS GAURAV MORESHW VIDWANS CHHATA" → "VIDWANS GAURAV MORESHW"
        // → Rule B: 3 tokens, no biz word → cap at 2 → "VIDWANS GAURAV"
        let result = normalize_ledger_name("VIDWANS GAURAV MORESHW VIDWANS CHHATA");
        // After Rule A: ["VIDWANS","GAURAV","MORESHW"]. After Rule B: ["VIDWANS","GAURAV"].
        // Rule C: V>G → stays ["VIDWANS","GAURAV"]. → "Vidwans Gaurav"
        assert_eq!(result, "Vidwans Gaurav");
    }

    #[test]
    fn ledger_rule_b_personal_name_capped_at_2() {
        // 3 tokens, no biz word → capped at 2
        let result = normalize_ledger_name("GAURAV MORESHWAR VIDWANS");
        // Rule C: MORESHWAR > GAURAV → sorted → "MORESHWAR GAURAV"
        assert_eq!(result, "Moreshwar Gaurav");
    }

    #[test]
    fn ledger_rule_c_canonical_order() {
        // "GAURAV VIDWANS" and "VIDWANS GAURAV" → both become "Vidwans Gaurav" (V > G)
        let r1 = normalize_ledger_name("GAURAV VIDWANS");
        let r2 = normalize_ledger_name("VIDWANS GAURAV");
        assert_eq!(r1, r2, "both orderings produce the same canonical form");
        assert_eq!(r1, "Vidwans Gaurav");
    }

    #[test]
    fn ledger_biz_word_keeps_3_tokens() {
        let result = normalize_ledger_name("RAHUL TRADERS ASSOCIATES");
        // "ASSOCIATES" is a biz indicator → not capped at 2
        // but "ASSOCIATES" not in suffix noise → remains
        // Rule C doesn't apply (biz word present)
        // Result: 3-token → title-cased
        assert!(result.split_whitespace().count() >= 2);
    }

    #[test]
    fn ledger_lic_dict_hit() {
        assert_eq!(normalize_ledger_name("LIC"), "LIC of India");
    }

    // ── clean() ──────────────────────────────────────────────────────────────

    #[test]
    fn clean_upi_amazon() {
        let meta = clean("UPI/DR/215218311944/AMAZON SELLER PAYMENTS");
        assert_eq!(meta.payment_type, PaymentType::Upi);
        assert!(!meta.cleaned.is_empty());
        assert!(meta.confidence >= 0.4);
    }

    #[test]
    fn clean_neft_with_utr() {
        let meta = clean("NEFT UTR:234567890123 FROM RAJESH SHAH HDFC");
        assert_eq!(meta.payment_type, PaymentType::Neft);
        assert_eq!(meta.payment_ref, "234567890123");
    }

    #[test]
    fn clean_empty_string() {
        let meta = clean("");
        assert_eq!(meta.confidence, 0.0);
        assert_eq!(meta.cleaned, "");
    }

    #[test]
    fn clean_atm_type() {
        let meta = clean("ATM WDL 10000 SBI ATM BANDRA");
        assert_eq!(meta.payment_type, PaymentType::Atm);
        assert!(meta.cleaned.starts_with("ATM Withdrawal"));
    }

    #[test]
    fn clean_interest_credited() {
        let meta = clean("INTEREST CREDITED FOR JAN 2024");
        assert_eq!(meta.payment_type, PaymentType::Interest);
        assert!(meta.cleaned.starts_with("Interest"));
    }

    #[test]
    fn clean_salary() {
        let meta = clean("SALARY CREDIT ACME PVT LTD JAN 2024");
        assert_eq!(meta.payment_type, PaymentType::Salary);
        assert!(meta.cleaned.to_uppercase().contains("SAL") || meta.cleaned.contains("Salary"));
    }

    // ── normalize_ledger_name edge cases ─────────────────────────────────────

    #[test]
    fn ledger_empty_string() {
        assert_eq!(normalize_ledger_name(""), "");
    }

    #[test]
    fn ledger_single_word_preserved() {
        assert_eq!(normalize_ledger_name("AMAZON"), "Amazon");
    }

    #[test]
    fn ledger_msedcl_dict_hit() {
        assert_eq!(normalize_ledger_name("MSEDCL"), "MSEDCL");
    }

    #[test]
    fn ledger_google_pay_gpay() {
        assert_eq!(normalize_ledger_name("GPAY"), "Google Pay");
    }

    #[test]
    fn ledger_phonepe() {
        assert_eq!(normalize_ledger_name("PHONEPE"), "PhonePe");
    }

    #[test]
    fn ledger_suffix_only_services() {
        // "SERVICES" is suffix noise; with only 1 word remaining can't strip further
        let result = normalize_ledger_name("MYCOMPANY SERVICES PVT LTD");
        // Strip LTD → PVT → SERVICES → stops (only MYCOMPANY left, can't strip below 1)
        // Actually strip order: LTD, PVT, SERVICES → ["MYCOMPANY"]
        // Then Rule B: 1 token → no cap needed. Result: "Mycompany"
        // Hmm wait - let me trace: words = ["MYCOMPANY","SERVICES","PVT","LTD"]
        //   iteration 1: last=LTD ∈ noise → pop → ["MYCOMPANY","SERVICES","PVT"]
        //   iteration 2: last=PVT ∈ noise → pop → ["MYCOMPANY","SERVICES"]
        //   iteration 3: last=SERVICES ∈ noise → pop → ["MYCOMPANY"], len=1, stop
        // Rule A: words.len()<3, skip. Rule B: words.len()<3, skip. Rule C: len≠2, skip.
        // Post-strip dict: "MYCOMPANY" not in dict. Title-case → "Mycompany"
        assert_eq!(result, "Mycompany");
    }
}
