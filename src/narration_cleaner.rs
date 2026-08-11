//! narration_cleaner.rs — Port of the Electron NarrationCleaner engine.
//!
//! Pipeline: detect type → strip noise tokens → extract party → normalize → score
//! Input:  raw bank narration string  e.g. "UPI/DR/2394823/AMAZON SELLER PAYMEN"
//! Output: NarrationMeta { original, cleaned, txn_type, party, payment_ref, confidence }

use once_cell::sync::Lazy;
use std::collections::HashSet;

// ── Payment type ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentType {
    Upi,
    Neft,
    Rtgs,
    Imps,
    Nach,
    Atm,
    Pos,
    Cheque,
    Cash,
    Interest,
    Charges,
    Transfer,
    Salary,
    Emi,
    Dd,
    Swift,
    Other,
}

impl PaymentType {
    fn prefix(&self) -> &'static str {
        match self {
            PaymentType::Upi => "UPI - ",
            PaymentType::Neft => "NEFT - ",
            PaymentType::Rtgs => "RTGS - ",
            PaymentType::Imps => "IMPS - ",
            PaymentType::Nach => "NACH/ACH - ",
            PaymentType::Atm => "ATM Withdrawal",
            PaymentType::Pos => "Card Payment - ",
            PaymentType::Cheque => "Cheque - ",
            PaymentType::Cash => "Cash",
            PaymentType::Interest => "Interest",
            PaymentType::Charges => "Bank Charges",
            PaymentType::Transfer => "Transfer - ",
            PaymentType::Salary => "Salary - ",
            PaymentType::Emi => "EMI - ",
            PaymentType::Dd => "Demand Draft - ",
            PaymentType::Swift => "SWIFT Transfer - ",
            PaymentType::Other => "",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NarrationMeta {
    pub original: String,
    pub cleaned: String,
    pub txn_type: String,
    pub party: String,
    pub payment_ref: String,
    pub confidence: f64,
}

/// Requirement #5 (imported vs system-generated coloring): does the Main
/// Screen's Narration cell actually end up showing this cleaned/derived text
/// instead of the bank's raw imported string?
///
/// Mirrors the exact condition `main.rs` already uses to pick which text to
/// display (Settings: "Preserve original narration" off, cleaner confident,
/// cleaned text non-empty) — kept as one named, tested function so the two
/// can never silently drift apart and show one thing while coloring another.
pub fn narration_display_is_generated(narr_preserve: bool, meta: &NarrationMeta) -> bool {
    !narr_preserve && meta.confidence >= 0.4 && !meta.cleaned.is_empty()
}

// ── Static data ──────────────────────────────────────────────────────────────

static VENDOR_DICT: Lazy<Vec<(&'static str, &'static str)>> = Lazy::new(|| {
    vec![
        ("AMAZON SELLER", "Amazon Seller"),
        ("AMAZON", "Amazon"),
        ("FLIPKART", "Flipkart"),
        ("SWIGGY INSTAMART", "Swiggy"),
        ("SWIGGY", "Swiggy"),
        ("ZOMATO", "Zomato"),
        ("AIRTEL", "Airtel"),
        ("RELIANCE JIO", "Reliance Jio"),
        ("JIO", "Reliance Jio"),
        ("VODAFONE", "Vodafone"),
        ("BSNL", "BSNL"),
        ("MSEDCL", "MSEDCL"),
        ("BESCOM", "BESCOM"),
        ("LIC", "LIC of India"),
        ("ICICI PRU", "ICICI Prudential"),
        ("HDFC LIFE", "HDFC Life"),
        ("MAX LIFE", "Max Life Insurance"),
        ("STAR HEALTH", "Star Health Insurance"),
        ("FACEBOOK", "Meta (Facebook)"),
        ("GOOGLE", "Google"),
        ("NETFLIX", "Netflix"),
        ("HOTSTAR", "Disney+ Hotstar"),
        ("SPOTIFY", "Spotify"),
        ("MICROSOFT", "Microsoft"),
        ("APPLE", "Apple"),
        ("UBER", "Uber"),
        ("OLA", "Ola"),
        ("RAPIDO", "Rapido"),
        ("IRCTC", "IRCTC"),
        ("MAKEMYTRIP", "MakeMyTrip"),
        ("GOIBIBO", "Goibibo"),
        ("MYNTRA", "Myntra"),
        ("NYKAA", "Nykaa"),
        ("BIGBASKET", "BigBasket"),
        ("BLINKIT", "Blinkit"),
        ("ZEPTO", "Zepto"),
        ("PAYTM", "Paytm"),
        ("PHONEPE", "PhonePe"),
        ("RAZORPAY", "Razorpay"),
        ("CASHFREE", "Cashfree"),
        ("GPAY", "Google Pay"),
        ("GPY", "Google Pay"),
        ("BPCL", "BPCL"),
        ("HPCL", "HPCL"),
        ("INDIAN OIL", "Indian Oil"),
        ("TATA", "Tata"),
        ("ELECTRICITY", "Electricity Board"),
        ("DUNZO", "Dunzo"),
        ("JUSPAY", "Juspay"),
    ]
});

static JUNK: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "NEFT",
        "RTGS",
        "IMPS",
        "UPI",
        "NACH",
        "ACH",
        "ECS",
        "ATM",
        "POS",
        "DD",
        "CHQ",
        "CLG",
        "CR",
        "DR",
        "CREDIT",
        "DEBIT",
        "INWARD",
        "OUTWARD",
        "TRF",
        "TRANSFER",
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
        "AND",
        "THE",
        "REF",
        "UTR",
        "TXN",
        "NO",
        "NUM",
        "ID",
        "TRNF",
        "PAYMENT",
        "PAID",
        "PAY",
        "RECEIVED",
        "RECV",
        "RCV",
        "SENT",
        "SEND",
        "ONLINE",
        "NET",
        "BANKING",
        "INB",
        "MB",
        "MOB",
        "BANK",
        "BRANCH",
        "IFSC",
        "MICR",
        "SWIFT",
        "DEP",
        "DEPOSIT",
        "WDL",
        "WITHDRAWAL",
        "WITH",
        "INT",
        "INTEREST",
        "CLEARING",
        "AMT",
        "AMOUNT",
        "BAL",
        "BALANCE",
        "CHARGES",
        "CHRGS",
        "CHGS",
        "LEVY",
        "FEE",
        "FEES",
        "SB",
        "CA",
        "OD",
        "FD",
        "RD",
        "SAVINGS",
        "CURRENT",
        "AC",
        "ACCT",
        "ACCOUNT",
        "INR",
        "RS",
        "P2P",
        "P2M",
        "P2B",
        "P2A",
    ]
    .iter()
    .copied()
    .collect()
});

static BANK_TOKENS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "HDFC", "HDFCBANK", "ICICI", "ICICIB", "SBI", "SBIN", "AXIS", "AXISB", "KOTAK", "PNB",
        "BOI", "BOB", "IOB", "CANARA", "UNION", "IDBI", "YES", "RBL", "FEDERAL", "INDUSIND", "UCO",
        "PAYTM", "PHONEPE", "GPAY", "CRED",
    ]
    .iter()
    .copied()
    .collect()
});

static LEDGER_SUFFIX_NOISE: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "LTD",
        "LIMITED",
        "PVT",
        "PRIVATE",
        "INC",
        "CORP",
        "CORPORATION",
        "LLC",
        "LLP",
        "INDIA",
        "INDIAN",
        "SERVICES",
        "SERVICE",
        "SOLUTIONS",
        "SOLUTION",
        "TECHNOLOGIES",
        "TECHNOLOGY",
        "TECH",
        "SYSTEMS",
        "SYSTEM",
        "ENTERPRISES",
        "ENTERPRISE",
        "GROUP",
        "HOLDINGS",
        "HOLDING",
        "PAY",
        "PAYMENT",
        "PAYMENTS",
        "SELLER",
        "SELLERS",
        "MARKETPLACE",
        "RETAIL",
        "STORE",
        "SHOP",
        "ONLINE",
        "DIGITAL",
        "MOBILE",
        "INSTAMART",
        "EXPRESS",
        "NOW",
        "QUICK",
    ]
    .iter()
    .copied()
    .collect()
});

static LEDGER_BIZ_WORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "TRADERS",
        "TRADING",
        "TRADE",
        "INDUSTRIES",
        "INDUSTRY",
        "MANUFACTURING",
        "DISTRIBUTORS",
        "DISTRIBUTOR",
        "EXPORTERS",
        "EXPORTER",
        "IMPORTERS",
        "IMPORTER",
        "AGENCY",
        "AGENCIES",
        "CONTRACTORS",
        "CONTRACTOR",
        "DEVELOPERS",
        "DEVELOPER",
        "BUILDERS",
        "BUILDER",
        "CONSTRUCTIONS",
        "CONSTRUCTION",
        "ASSOCIATES",
        "ASSOCIATE",
        "PARTNERS",
        "PARTNER",
        "BROTHERS",
        "BROS",
        "CO",
        "COMPANY",
        "WORKS",
        "CONSULTANTS",
        "CONSULTANCY",
        "MART",
        "CENTRE",
        "CENTER",
        "HOUSE",
        "DEALERS",
        "DEALER",
        "SUPPLIERS",
        "SUPPLIER",
        "HOSPITAL",
        "CLINIC",
        "SCHOOL",
        "COLLEGE",
        "INSTITUTE",
        "FOUNDATION",
        "TRUST",
        "BANK",
    ]
    .iter()
    .copied()
    .collect()
});

// ── Detection ─────────────────────────────────────────────────────────────────

fn detect_type(narr: &str) -> PaymentType {
    let up = narr.to_uppercase();
    if up.contains("UPI") {
        return PaymentType::Upi;
    }
    if up.contains("NEFT") {
        return PaymentType::Neft;
    }
    if up.contains("RTGS") {
        return PaymentType::Rtgs;
    }
    if up.contains("IMPS") {
        return PaymentType::Imps;
    }
    if up.contains("NACH") || up.contains("ACH") || up.contains("ECS") {
        return PaymentType::Nach;
    }
    if up.contains("ATM") {
        return PaymentType::Atm;
    }
    if up.contains("POS") || up.contains("SWIPE") {
        return PaymentType::Pos;
    }
    if up.contains("CHQ") || up.contains("CHEQUE") || up.contains("CLG") {
        return PaymentType::Cheque;
    }
    if up.contains("SALARY") || up.contains(" SAL ") {
        return PaymentType::Salary;
    }
    if up.contains("EMI") {
        return PaymentType::Emi;
    }
    if up.contains("INTEREST") || up.contains(" INT ") {
        return PaymentType::Interest;
    }
    if up.contains("CHRG") || up.contains("CHGS") || up.contains("CHARGES") || up.contains("LEVY") {
        return PaymentType::Charges;
    }
    if up.contains("CASH") {
        return PaymentType::Cash;
    }
    if up.contains("TRF") || up.contains("TRANSFER") {
        return PaymentType::Transfer;
    }
    if up.contains("SWIFT") || up.contains("FOREIGN") {
        return PaymentType::Swift;
    }
    if up.contains(" DD ") || up.contains("DEMAND DRAFT") {
        return PaymentType::Dd;
    }
    PaymentType::Other
}

fn extract_ref(narr: &str) -> String {
    // UTR label
    if let Some(cap) = regex_first(narr, r"UTR[:\s]*([A-Z0-9]{10,22})") {
        return cap;
    }
    if let Some(cap) = regex_first(narr, r"REF[:\s#]*([A-Z0-9]{8,20})") {
        return cap;
    }
    // Long pure-numeric UTR (12-22 digits)
    if let Some(cap) = regex_first(narr, r"\b([0-9]{12,22})\b") {
        return cap;
    }
    // Cheque number
    if let Some(cap) = regex_first(narr, r"(?:CHQ|CHEQUE)[:\s]*([0-9]{6,9})") {
        return cap;
    }
    String::new()
}

fn regex_first(text: &str, pattern: &str) -> Option<String> {
    use regex::Regex;
    let re = Regex::new(pattern).ok()?;
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

// ── Noise stripping ───────────────────────────────────────────────────────────

fn strip_noise(narr: &str) -> String {
    let mut s = narr.to_string();

    // Strip URLs and VPA handles
    s = regex_replace_all(&s, r"https?://\S+", " ");
    s = regex_replace_all(&s, r"\S+@[a-z]+\b", " ");

    // Split slash/pipe/backslash segments, filter junk
    let parts: Vec<&str> = s
        .split(['/', '|', '\\'])
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    let kept: Vec<&str> = parts
        .iter()
        .filter(|p| {
            let up = p.to_uppercase();
            let up = up.trim();
            if up.is_empty() || up.len() <= 1 {
                return false;
            }
            if JUNK.contains(up) {
                return false;
            }
            if up.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            // IFSC pattern
            if up.len() == 11
                && up.chars().take(4).all(|c| c.is_ascii_uppercase())
                && up.chars().nth(4) == Some('0')
            {
                return false;
            }
            true
        })
        .copied()
        .collect();

    let mut joined = kept.join(" ");

    // Strip long numeric tokens (UTR/txn IDs)
    joined = regex_replace_all(&joined, r"\b[0-9]{6,22}\b", " ");
    // Strip long alphanumeric refs
    joined = regex_replace_all(&joined, r"\b[A-Z]{2,4}[0-9]{6,}\b", " ");
    // Strip IFSC-style
    joined = regex_replace_all(&joined, r"\b[A-Z]{4}0[A-Z0-9]{6}\b", " ");

    // Filter individual words
    let words: Vec<String> = joined
        .split_whitespace()
        .filter(|w| {
            let up = w.to_uppercase();
            !up.is_empty()
                && up.len() > 1
                && !up.chars().all(|c| c.is_ascii_digit())
                && !JUNK.contains(up.as_str())
        })
        .map(|w| w.to_string())
        .collect();

    words.join(" ")
}

fn regex_replace_all(text: &str, pattern: &str, replacement: &str) -> String {
    use regex::Regex;
    match Regex::new(pattern) {
        Ok(re) => re.replace_all(text, replacement).into_owned(),
        Err(_) => text.to_string(),
    }
}

// ── Party extraction ──────────────────────────────────────────────────────────

fn extract_party(stripped: &str) -> String {
    if stripped.is_empty() {
        return String::new();
    }

    let up = stripped.to_uppercase();

    // Vendor dict: longest match first
    for (k, canonical) in VENDOR_DICT.iter() {
        if up.starts_with(k) || up.contains(k) {
            return canonical.to_string();
        }
    }

    // Score words
    let words: Vec<&str> = stripped
        .split_whitespace()
        .filter(|w| w.len() > 1 && !w.chars().all(|c| c.is_ascii_digit()))
        .collect();

    if words.is_empty() {
        return String::new();
    }

    let score_word = |w: &str| -> i32 {
        let wu = w.to_uppercase();
        let mut sc: i32 = 3;
        if JUNK.contains(wu.as_str()) {
            sc -= 5;
        }
        if BANK_TOKENS.contains(wu.as_str()) {
            sc -= 3;
        }
        if w.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            sc -= 4;
        }
        if w.len() > 5 {
            sc += (w.len() as i32 - 5).min(3);
        }
        sc
    };

    let mut best = String::new();
    let mut best_score: i32 = 0;

    for i in 0..words.len() {
        let mut span = String::new();
        let mut span_score: i32 = 0;
        for w in words.iter().take((i + 5).min(words.len())).skip(i) {
            let wu = w.to_uppercase();
            if JUNK.contains(wu.as_str()) {
                break;
            }
            if w.chars().all(|c| c.is_ascii_digit()) {
                break;
            }
            if !span.is_empty() {
                span.push(' ');
            }
            span.push_str(w);
            span_score += score_word(w);
        }
        if span_score > best_score {
            best = span;
            best_score = span_score;
        }
    }

    best.chars().take(40).collect()
}

// ── Title case ────────────────────────────────────────────────────────────────

pub fn to_title_case(s: &str) -> String {
    let lower_words: HashSet<&str> = [
        "of", "in", "at", "for", "and", "or", "by", "to", "the", "a", "an",
    ]
    .iter()
    .copied()
    .collect();
    s.split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            if w.is_empty() {
                return w.to_string();
            }
            // Keep short all-caps abbreviations ≤4 chars (e.g. HDFC, RBL, GST)
            if w.len() <= 4
                && w.chars().all(|c| c.is_ascii_uppercase())
                && !lower_words.contains(w.to_lowercase().as_str())
            {
                return w.to_string();
            }
            let wl = w.to_lowercase();
            if lower_words.contains(wl.as_str()) && i > 0 {
                return wl;
            }
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Ledger name normalization ─────────────────────────────────────────────────

/// Strip a trailing "- branch/location" style suffix from a party name.
///
/// A hyphen with whitespace on at least one side is treated as a separator —
/// bank narrations commonly append a branch/city after one, e.g.
/// "ABC Traders- Mumbai" or "ABC Traders - Pune Branch" — so that suffix is
/// dropped, leaving just the core name. A hyphen with no adjacent whitespace
/// (e.g. "CO-OP", "WI-FI") is left untouched, since that's very likely part
/// of the name itself rather than a separator, and merging on it would risk
/// conflating two differently-named vendors.
///
/// Byte-indexed scanning is safe here even for multi-byte UTF-8 input: '-'
/// and ' ' are both single-byte ASCII, so comparing raw bytes never produces
/// a false match against a UTF-8 continuation byte, and slicing at the
/// hyphen's index is always on a char boundary.
fn strip_location_suffix(s: &str) -> &str {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'-' {
            continue;
        }
        let before_ws = i > 0 && bytes[i - 1] == b' ';
        let after_ws = i + 1 < bytes.len() && bytes[i + 1] == b' ';
        if before_ws || after_ws {
            return s[..i].trim_end();
        }
    }
    s
}

pub fn normalize_ledger_name(raw: &str) -> String {
    if raw.is_empty() {
        return raw.to_string();
    }
    // Drop a trailing location/branch suffix and abbreviation dots before the
    // dictionary/token pipeline below, so "ABC Traders- Mumbai" and
    // "A.B.C. Traders" resolve to the same canonical form as "ABC Traders".
    // Dots are deleted outright (not turned into spaces) so "A.B.C." fuses
    // into the single token "ABC" rather than splitting into "A", "B", "C".
    let no_suffix = strip_location_suffix(raw);
    let no_dots = no_suffix.replace('.', "");
    let name_up = no_dots.to_uppercase().trim().to_string();

    // Pre-strip dict check
    for (k, canonical) in VENDOR_DICT.iter() {
        if name_up == *k || name_up.starts_with(&format!("{} ", k)) {
            return canonical.to_string();
        }
    }

    // Strip suffix noise words (right end only)
    let mut words: Vec<&str> = name_up.split_whitespace().collect();
    let mut changed = true;
    while changed && words.len() > 1 {
        changed = false;
        if let Some(last) = words.last() {
            if LEDGER_SUFFIX_NOISE.contains(*last) {
                words.pop();
                changed = true;
            }
        }
    }

    // Rule A: Repeated leading-token collapse
    if words.len() >= 3 {
        let leading: HashSet<&str> = words[..2].iter().copied().collect();
        let mut cut = words.len();

        for (i, w) in words.iter().copied().enumerate().skip(2) {
            if leading.contains(w) {
                cut = i;
                break;
            }
        }

        words.truncate(cut);
    }

    // Rule B: Cap at 2 tokens for non-business names
    if words.len() >= 3 && !words.iter().any(|w| LEDGER_BIZ_WORDS.contains(*w)) {
        words.truncate(2);
    }

    // Rule C: Canonical token order for 2-token personal names
    if words.len() == 2
        && !words.iter().any(|w| LEDGER_BIZ_WORDS.contains(*w))
        && words[0].cmp(words[1]) == std::cmp::Ordering::Less
    {
        words.swap(0, 1);
    }

    let stripped = words.join(" ");

    // Post-strip dict check
    for (k, canonical) in VENDOR_DICT.iter() {
        if stripped == *k || stripped.starts_with(&format!("{} ", k)) {
            return canonical.to_string();
        }
    }

    to_title_case(&stripped)
}

// ── Build cleaned narration string ────────────────────────────────────────────

fn build_cleaned(party: &str, ptype: &PaymentType, stripped: &str) -> String {
    let pre = ptype.prefix();
    let name = if party.is_empty() {
        stripped.chars().take(40).collect::<String>()
    } else {
        party.to_string()
    };
    if name.is_empty() {
        return "Unknown".to_string();
    }

    match ptype {
        PaymentType::Atm | PaymentType::Cash | PaymentType::Interest | PaymentType::Charges => {
            if party.is_empty() {
                pre.to_string()
            } else {
                format!("{} - {}", pre, party)
            }
        }
        _ => {
            if pre.is_empty() {
                name
            } else {
                format!("{}{}", pre, name)
            }
        }
    }
}

// ── Confidence ────────────────────────────────────────────────────────────────

fn score(original: &str, cleaned: &str, party: &str, ptype: &PaymentType) -> f64 {
    let mut sc: f64 = 0.4;
    if *ptype != PaymentType::Other {
        sc += 0.15;
    }
    if party.len() >= 3 {
        sc += 0.15;
    }
    if party.len() >= 6 {
        sc += 0.10;
    }
    let up = party.to_uppercase();
    if VENDOR_DICT
        .iter()
        .any(|(k, _)| up == *k || up.starts_with(k))
    {
        sc += 0.15;
    }
    let ratio = cleaned.len() as f64 / original.len().max(1) as f64;
    if ratio < 0.8 {
        sc += 0.05;
    }
    (sc * 100.0).round() / 100.0
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Clean a single narration string.
///
/// Equivalent to `clean_with(raw, true)` — kept as the default entry point so
/// existing callers (and tests) that don't care about the Settings screen's
/// "Convert to Title Case" toggle keep their exact prior behavior.
pub fn clean(raw: &str) -> NarrationMeta {
    clean_with(raw, true)
}

/// Clean a single narration string, honoring the "Convert to Title Case"
/// setting (`Settings.narr_title_case`). Mirrors the old Electron engine's
/// `useTitle` flag: `_toTitleCase()` is skipped on the extracted party and
/// stripped text when `title_case` is false, but the low-confidence fallback
/// (which title-cases a truncated slice of the raw original) still applies
/// regardless — that asymmetry matches the original engine exactly.
pub fn clean_with(raw: &str, title_case: bool) -> NarrationMeta {
    let original = raw.trim().to_string();
    if original.is_empty() {
        return NarrationMeta {
            original,
            cleaned: String::new(),
            txn_type: "OTHER".to_string(),
            party: String::new(),
            payment_ref: String::new(),
            confidence: 0.0,
        };
    }

    let ptype = detect_type(&original);
    let payment_ref = extract_ref(&original);
    let stripped = strip_noise(&original);
    let party_raw = extract_party(&stripped);
    let party = if title_case {
        to_title_case(&party_raw)
    } else {
        party_raw
    };
    let stripped_display = if title_case {
        to_title_case(&stripped)
    } else {
        stripped.clone()
    };
    let cleaned_str = build_cleaned(&party, &ptype, &stripped_display);
    let confidence = score(&original, &cleaned_str, &party, &ptype).min(0.99);

    let txn_type = format!("{:?}", ptype)
        .to_uppercase()
        .replace("PAYMENTTYPE::", "");

    let final_cleaned = if confidence >= 0.4 {
        cleaned_str.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        to_title_case(&original.chars().take(60).collect::<String>())
    };

    NarrationMeta {
        original,
        cleaned: final_cleaned,
        txn_type,
        party,
        payment_ref,
        confidence,
    }
}

/// Clean a batch of transactions, returning cleaned narration strings.
/// Returns `(cleaned_narration, party_suggestion)` for each input.
pub fn clean_batch(narrations: &[String]) -> Vec<NarrationMeta> {
    clean_batch_with(narrations, true)
}

/// Batch form of [`clean_with`] — see its docs for what `title_case` controls.
pub fn clean_batch_with(narrations: &[String], title_case: bool) -> Vec<NarrationMeta> {
    narrations
        .iter()
        .map(|n| clean_with(n, title_case))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── narration_display_is_generated (Requirement #5) ───────────────────────

    fn meta_with(confidence: f64, cleaned: &str) -> NarrationMeta {
        NarrationMeta {
            original: "orig".to_string(),
            cleaned: cleaned.to_string(),
            txn_type: "Other".to_string(),
            party: String::new(),
            payment_ref: String::new(),
            confidence,
        }
    }

    #[test]
    fn narr_preserve_on_always_shows_raw_narration() {
        // "Preserve original narration" wins even when the cleaner is
        // fully confident — the Narration column must read as imported.
        let meta = meta_with(1.0, "Cleaned Vendor Name");
        assert!(!narration_display_is_generated(true, &meta));
    }

    #[test]
    fn low_confidence_cleaning_keeps_raw_narration() {
        let meta = meta_with(0.2, "Cleaned Vendor Name");
        assert!(!narration_display_is_generated(false, &meta));
    }

    #[test]
    fn empty_cleaned_text_keeps_raw_narration() {
        let meta = meta_with(0.9, "");
        assert!(!narration_display_is_generated(false, &meta));
    }

    #[test]
    fn confident_non_empty_cleaning_is_shown_as_generated() {
        let meta = meta_with(0.9, "Cleaned Vendor Name");
        assert!(narration_display_is_generated(false, &meta));
    }

    #[test]
    fn confidence_exactly_at_threshold_counts_as_generated() {
        let meta = meta_with(0.4, "Cleaned Vendor Name");
        assert!(narration_display_is_generated(false, &meta));
    }

    #[test]
    fn detects_upi() {
        let m = clean("UPI/DR/2394823/AMAZON SELLER PAYMEN/AxisB");
        assert_eq!(m.txn_type, "UPI");
    }

    #[test]
    fn extracts_amazon_party() {
        let m = clean("UPI/DR/2394823/AMAZON SELLER PAYMEN/AxisB");
        assert!(
            m.party.to_lowercase().contains("amazon"),
            "got: {}",
            m.party
        );
    }

    #[test]
    fn strips_long_numeric_utr() {
        let m = clean("NEFT CR 241806834723 HDFC BANK");
        assert!(!m.cleaned.contains("241806834723"), "got: {}", m.cleaned);
    }

    #[test]
    fn normalize_ledger_amazon_pay() {
        let n = normalize_ledger_name("AMAZON PAY");
        assert_eq!(n, "Amazon");
    }

    #[test]
    fn normalize_ledger_personal_name() {
        let n = normalize_ledger_name("GAURAV VIDWANS");
        // canonical order: V > G → VIDWANS GAURAV
        assert_eq!(n, "Vidwans Gaurav");
    }

    // ── Requirement #1 ("Club All Customer / Vendor Names") ─────────────────
    // "ABC Traders", "ABC TRADERS", "ABC Traders Pvt Ltd", "A.B.C. Traders",
    // and "ABC Traders- Mumbai" must all normalize to the same canonical form.

    #[test]
    fn normalize_ledger_case_difference_collapses() {
        assert_eq!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("ABC TRADERS")
        );
    }

    #[test]
    fn normalize_ledger_legal_suffix_collapses() {
        assert_eq!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("ABC Traders Pvt Ltd")
        );
    }

    #[test]
    fn normalize_ledger_abbreviation_dots_collapse() {
        assert_eq!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("A.B.C. Traders")
        );
    }

    #[test]
    fn normalize_ledger_trailing_location_suffix_collapses() {
        assert_eq!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("ABC Traders- Mumbai")
        );
        assert_eq!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("ABC Traders - Pune Branch")
        );
    }

    #[test]
    fn normalize_ledger_hyphen_without_whitespace_is_not_a_location_separator() {
        // "CO-OP" has no space on either side of the hyphen — that's very
        // likely part of the name itself, not a "name - location" separator,
        // so it must survive untouched rather than being truncated to "CO".
        assert_eq!(strip_location_suffix("XYZ CO-OP"), "XYZ CO-OP");
    }

    #[test]
    fn normalize_ledger_does_not_merge_different_business_names() {
        // Same business-word suffix, different core name — must stay distinct.
        assert_ne!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("XYZ Traders")
        );
        // Same prefix, different business-type word — must stay distinct.
        assert_ne!(
            normalize_ledger_name("ABC Traders"),
            normalize_ledger_name("ABC Distributors")
        );
    }

    #[test]
    fn to_title_case_basic() {
        assert_eq!(to_title_case("AIRTEL INDIA"), "Airtel India");
        assert_eq!(to_title_case("HDFC BANK"), "HDFC BANK");
    }

    // ── Settings wiring: narr_title_case ──────────────────────────────────────

    // "RAMESH KUMAR" is deliberately not in VENDOR_DICT (unlike e.g. "TATA" or
    // "AMAZON", which short-circuit extract_party() to a fixed canonical
    // string regardless of title_case) — its word-scoring path returns the
    // raw uppercase words verbatim, so to_title_case() actually has something
    // to do, making it a real test of the title_case flag rather than a no-op.

    #[test]
    fn clean_with_title_case_true_matches_default_clean() {
        let narr = "UPI/CR/234567890123/RAMESH KUMAR";
        assert_eq!(clean_with(narr, true).cleaned, clean(narr).cleaned);
        assert_eq!(clean_with(narr, true).party, clean(narr).party);
    }

    #[test]
    fn clean_with_title_case_false_keeps_upper_case_party() {
        let narr = "UPI/CR/234567890123/RAMESH KUMAR";
        let titled = clean_with(narr, true);
        let untitled = clean_with(narr, false);
        assert_eq!(titled.party, "Ramesh Kumar");
        assert_eq!(
            untitled.party, "RAMESH KUMAR",
            "title_case=false must skip to_title_case on the party"
        );
        assert_ne!(titled.cleaned, untitled.cleaned);
    }

    #[test]
    fn clean_batch_with_threads_title_case_through_every_entry() {
        let batch = vec![
            "UPI/CR/234567890123/RAMESH KUMAR".to_string(),
            "UPI/DR/2394823/AMAZON SELLER PAYMEN/AxisB".to_string(),
        ];
        let untitled = clean_batch_with(&batch, false);
        assert_eq!(untitled[0].party, "RAMESH KUMAR");
        // Amazon comes from the vendor dict's canonical spelling, not from
        // to_title_case(), so it's unaffected by the flag either way.
        assert!(untitled[1].party.to_uppercase().contains("AMAZON"));
    }
}
