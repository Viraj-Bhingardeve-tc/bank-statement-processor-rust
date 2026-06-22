//! gst_engine.rs — Port of Electron's GSTEngine: intelligent GST-aware ledger
//! classification. Detects GST from narrations, classifies CGST/SGST/IGST,
//! suggests input/output tax ledgers, extracts GSTINs, and estimates GST amounts.
//!
//! This is a richer analysis than `classifier::detect_gst` (which only tags
//! "GST"/"TAX" from narration keywords) — it also recognizes GST-applicable
//! vendors by name alone (e.g. "AIRTEL" implies an 18% GST expense) and can
//! suggest an expense ledger when the account head is still blank.

use once_cell::sync::Lazy;
use regex::Regex;

// ── GST rate detection patterns ───────────────────────────────────────────────

static RATE_PATTERNS: Lazy<Vec<(f64, Regex)>> = Lazy::new(|| vec![
    (28.0, Regex::new(r"(?i)\b28\s*%?\s*(gst|tax)\b|gst\s*@?\s*28\b").unwrap()),
    (18.0, Regex::new(r"(?i)\b18\s*%?\s*(gst|tax)\b|gst\s*@?\s*18\b").unwrap()),
    (12.0, Regex::new(r"(?i)\b12\s*%?\s*(gst|tax)\b|gst\s*@?\s*12\b").unwrap()),
    (5.0,  Regex::new(r"(?i)\b5\s*%?\s*(gst|tax)\b|gst\s*@?\s*5\b").unwrap()),
]);

static RE_IGST: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bIGST\b").unwrap());
static RE_CGST: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bCGST\b").unwrap());
static RE_SGST: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bSGST\b|\bUTGST\b").unwrap());
static RE_GST_MENTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bGST\b|\bIGST\b|\bCGST\b|\bSGST\b").unwrap());
static RE_GSTIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[0-9]{2}[A-Z]{5}[0-9]{4}[A-Z][1-9A-Z]Z[0-9A-Z]").unwrap()
});

// ── Known GST-registered vendor categories ────────────────────────────────────

struct VendorGst {
    kw:      &'static [&'static str],
    rate:    f64,
    gst_type: &'static str,
    expense: &'static str,
}

static VENDOR_GST_MAP: Lazy<Vec<VendorGst>> = Lazy::new(|| vec![
    VendorGst { kw: &["AIRTEL","JIO","BSNL","VODAFONE","VI ","IDEA","TATA TELE"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Telephone Expense" },
    VendorGst { kw: &["INTERNET","BROADBAND","FIBER","EXCITEL","HATHWAY","ACT FIBER","RAILWIRE"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Internet Charges" },
    VendorGst { kw: &["MICROSOFT","GOOGLE","AWS","AMAZON WEB","AZURE","ZOHO","SALESFORCE",
        "FRESHDESK","HUBSPOT","QUICKBOOKS","NOTION","SLACK","ZOOM","WEBEX"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Software Expense" },
    VendorGst { kw: &["MSEDCL","BESCOM","TNEB","KSEB","CESC","WBSEDCL","TORRENT POWER",
        "ADANI ELECTRICITY","ELECTRICITY BOARD","POWER SUPPLY"],
        rate: 5.0, gst_type: "CGST+SGST", expense: "Electricity Charges" },
    VendorGst { kw: &["INSURANCE","LIC","SBI LIFE","HDFC LIFE","ICICI PRU","MAX LIFE",
        "BAJAJ ALLIANZ","STAR HEALTH","MEDICLAIM","POLICY"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Insurance Premium" },
    VendorGst { kw: &["CONSULTANT","AUDIT","LEGAL","ADVOCATE","CHARTERED","CA FIRM","LAWYER"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Professional Fees" },
    VendorGst { kw: &["RENT","OFFICE RENT","SHOP RENT","RENTAL","LANDLORD"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Rent" },
    VendorGst { kw: &["COURIER","DTDC","BLUEDART","DELHIVERY","AMAZON SHIP","LOGISTIC",
        "TRANSPORT","FREIGHT"],
        rate: 5.0, gst_type: "CGST+SGST", expense: "Courier & Freight" },
    VendorGst { kw: &["SWIGGY","ZOMATO","RESTAURANT","HOTEL","CAFE","DHABA","BARBEQUE"],
        rate: 5.0, gst_type: "CGST+SGST", expense: "Food Expense" },
    VendorGst { kw: &["PETROL","DIESEL","BPCL","HPCL","INDIAN OIL","HP PETRO","FUEL PUMP"],
        rate: 0.0, gst_type: "EXEMPT", expense: "Fuel Expense" },
    VendorGst { kw: &["UBER","OLA","RAPIDO","MERU","TAXISURE"],
        rate: 5.0, gst_type: "CGST+SGST", expense: "Travelling Expense" },
    VendorGst { kw: &["FACEBOOK ADS","GOOGLE ADS","META ADS","INSTAGRAM ADS","LINKEDIN ADS"],
        rate: 18.0, gst_type: "CGST+SGST", expense: "Advertisement Expense" },
]);

// ── Helpers ────────────────────────────────────────────────────────────────────

fn detect_rate(text: &str) -> Option<f64> {
    RATE_PATTERNS.iter().find(|(_, re)| re.is_match(text)).map(|(r, _)| *r)
}

fn detect_component(text: &str) -> Option<&'static str> {
    if RE_IGST.is_match(text) { return Some("IGST"); }
    if RE_CGST.is_match(text) || RE_SGST.is_match(text) { return Some("CGST+SGST"); }
    None
}

/// Extract all (de-duplicated) GSTINs found in `text`.
pub fn extract_gstins(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in RE_GSTIN.find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) { out.push(s); }
    }
    out
}

fn vendor_match(text_upper: &str) -> Option<&'static VendorGst> {
    VENDOR_GST_MAP.iter().find(|v| v.kw.iter().any(|k| text_upper.contains(k)))
}

/// Suggested input/output tax ledger label (display string), or `None` for exempt.
fn suggest_ledger(gst_type: &str, rate: f64, is_expense: bool) -> Option<String> {
    if gst_type == "EXEMPT" || rate == 0.0 { return None; }
    if gst_type == "IGST" {
        return Some(if is_expense {
            format!("IGST Input ({}%)", rate)
        } else {
            format!("IGST Output ({}%)", rate)
        });
    }
    let half = rate / 2.0;
    Some(if is_expense {
        format!("CGST Input ({}%) + SGST Input ({}%)", half, half)
    } else {
        format!("CGST Output ({}%) + SGST Output ({}%)", half, half)
    })
}

#[derive(Debug, Clone)]
pub struct GstAnalysis {
    pub gst_rate:        Option<f64>,
    pub gst_type:         Option<String>,   // "IGST" | "CGST+SGST" | "EXEMPT"
    pub gst_amount:       Option<f64>,       // estimated tax portion
    pub gstins:           Vec<String>,
    pub expense_ledger:   Option<String>,
    pub suggested_ledger: Option<String>,
    pub confidence:       f64,
}

/// Analyse a transaction's narration/reference/vendor text for GST signal.
/// Returns `None` if nothing GST-related is detected (mirrors `_analyseTxn`).
pub fn analyse(narration: &str, reference: &str, vendor: &str, debit: Option<f64>, credit: Option<f64>) -> Option<GstAnalysis> {
    let text = format!("{} {} {}", narration, reference, vendor);
    let text_upper = text.to_uppercase();
    let is_expense = debit.is_some() && credit.is_none();

    let gstins      = extract_gstins(&text);
    let component   = detect_component(&text);
    let rate_from_txt = detect_rate(&text);
    let vendor_hit  = vendor_match(&text_upper);
    let gst_mentioned = RE_GST_MENTION.is_match(&text);

    if !gst_mentioned && vendor_hit.is_none() && gstins.is_empty() && rate_from_txt.is_none() {
        return None;
    }

    let gst_rate = rate_from_txt.or(vendor_hit.map(|v| v.rate));
    let gst_type = component.map(|s| s.to_string())
        .or_else(|| vendor_hit.map(|v| v.gst_type.to_string()))
        .or_else(|| if gst_rate.is_some() { Some("CGST+SGST".to_string()) } else { None });

    let base = debit.or(credit);
    let gst_amount = match (base, gst_rate) {
        (Some(b), Some(r)) if r > 0.0 => Some(((b * r / (100.0 + r)) * 100.0).round() / 100.0),
        _ => None,
    };

    let suggested_ledger = match (&gst_type, gst_rate) {
        (Some(gt), Some(r)) => suggest_ledger(gt, r, is_expense),
        _ => None,
    };

    let mut confidence: f64 = 0.0;
    if !gstins.is_empty()    { confidence += 0.35; }
    if component.is_some()   { confidence += 0.20; }
    if rate_from_txt.is_some() { confidence += 0.20; }
    if vendor_hit.is_some()  { confidence += 0.15; }
    if gst_mentioned         { confidence += 0.10; }
    let confidence = (confidence * 100.0).round() / 100.0;
    let confidence = confidence.min(0.99);

    Some(GstAnalysis {
        gst_rate,
        gst_type,
        gst_amount,
        gstins,
        expense_ledger: vendor_hit.map(|v| v.expense.to_string()),
        suggested_ledger,
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_explicit_igst() {
        let a = analyse("IGST PAYMENT TO VENDOR", "", "", Some(1000.0), None).unwrap();
        assert_eq!(a.gst_type.as_deref(), Some("IGST"));
    }

    #[test]
    fn detects_vendor_without_gst_keyword() {
        let a = analyse("AIRTEL POSTPAID BILL", "", "", Some(999.0), None).unwrap();
        assert_eq!(a.gst_rate, Some(18.0));
        assert_eq!(a.expense_ledger.as_deref(), Some("Telephone Expense"));
    }

    #[test]
    fn no_match_returns_none() {
        assert!(analyse("SALARY CREDIT", "", "", None, Some(50000.0)).is_none());
    }

    #[test]
    fn extracts_gstin() {
        let g = extract_gstins("Invoice 27AAAPL1234C1ZV paid");
        assert_eq!(g, vec!["27AAAPL1234C1ZV".to_string()]);
    }

    #[test]
    fn exempt_fuel_has_no_ledger() {
        let a = analyse("BPCL PETROL PUMP", "", "", Some(2000.0), None).unwrap();
        assert_eq!(a.gst_type.as_deref(), Some("EXEMPT"));
        assert!(a.suggested_ledger.is_none());
    }

    #[test]
    fn gst_amount_extracted_inclusive() {
        // 18% inclusive of 1180 -> tax = 1180*18/118 = 180.0
        let a = analyse("SOFTWARE SUBSCRIPTION GST 18", "", "", Some(1180.0), None).unwrap();
        assert!((a.gst_amount.unwrap() - 180.0).abs() < 0.5);
    }
}
