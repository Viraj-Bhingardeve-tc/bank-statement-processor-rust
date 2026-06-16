// analytics.rs — Dashboard analytics: mirrors JS AnalyticsEngine.compute()
// Pure data aggregation — no UI, no side-effects.

use crate::parser::Transaction;

// ── Date helpers ──────────────────────────────────────────────────────────────

fn parse_dd_mm_yyyy(s: &str) -> Option<(u32, u32, i32)> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 3 { return None; }
    let dd = parts[0].parse::<u32>().ok()?;
    let mm = parts[1].parse::<u32>().ok()?;
    let yyyy = parts[2].parse::<i32>().ok()?;
    Some((dd, mm, yyyy))
}

fn month_key(date: &str) -> Option<String> {
    let (_, mm, yyyy) = parse_dd_mm_yyyy(date)?;
    Some(format!("{:04}-{:02}", yyyy, mm))
}

fn month_label(key: &str) -> String {
    let names = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() != 2 { return key.to_string(); }
    let m = parts[1].parse::<usize>().unwrap_or(1).saturating_sub(1);
    let y = parts[0];
    format!("{} {}", names.get(m).unwrap_or(&""), &y[2..])  // "Jan 24"
}

// INR formatter: ₹ X,XX,XXX.XX
pub fn fmt_inr(v: f64) -> String {
    let rounded = (v * 100.0).round() / 100.0;
    let (sign, abs) = if rounded < 0.0 { ("-", -rounded) } else { ("", rounded) };
    let int_part  = abs as u64;
    let frac_part = ((abs - int_part as f64) * 100.0).round() as u64;
    let s = int_part.to_string();
    let formatted = if s.len() <= 3 {
        s.clone()
    } else {
        let (first, rest) = s.split_at(s.len() - 3);
        let mut parts = vec![rest.to_string()];
        let mut remaining = first;
        while remaining.len() > 2 {
            let (r, chunk) = remaining.split_at(remaining.len() - 2);
            parts.push(chunk.to_string());
            remaining = r;
        }
        if !remaining.is_empty() { parts.push(remaining.to_string()); }
        parts.reverse();
        parts.join(",")
    };
    format!("₹ {}{}.{:02}", sign, formatted, frac_part)
}

fn fmt_short(v: f64) -> String {
    if v >= 1_000_000.0 { return format!("₹{:.1}L", v / 100_000.0); }
    if v >= 1_000.0    { return format!("₹{:.0}K", v / 1_000.0);   }
    fmt_inr(v)
}

// ── Filter ────────────────────────────────────────────────────────────────────

pub struct DashFilter<'a> {
    pub from:    &'a str,
    pub to:      &'a str,
    pub bank:    &'a str,
    pub vendor:  &'a str,
    pub head:    &'a str,
}

impl<'a> DashFilter<'a> {
    pub fn is_empty(&self) -> bool {
        self.from.is_empty() && self.to.is_empty()
            && self.bank.is_empty()
            && self.vendor.is_empty()
            && self.head.is_empty()
    }
}

pub fn filter_txns<'t>(txns: &'t [Transaction], f: &DashFilter) -> Vec<&'t Transaction> {
    txns.iter().filter(|t| {
        if t.is_opening_balance { return false; }
        if !f.bank.is_empty()   && t.bank_name != f.bank   { return false; }
        if !f.vendor.is_empty() && t.vendor    != f.vendor { return false; }
        if !f.head.is_empty()   && t.account_head != f.head { return false; }
        if !f.from.is_empty() || !f.to.is_empty() {
            if let Some((dd, mm, yyyy)) = parse_dd_mm_yyyy(&t.date) {
                // from/to in DD/MM/YYYY too
                if !f.from.is_empty() {
                    if let Some((fdd, fmm, fyyyy)) = parse_dd_mm_yyyy(f.from) {
                        if (yyyy, mm, dd) < (fyyyy, fmm, fdd) { return false; }
                    }
                }
                if !f.to.is_empty() {
                    if let Some((tdd, tmm, tyyyy)) = parse_dd_mm_yyyy(f.to) {
                        if (yyyy, mm, dd) > (tyyyy, tmm, tdd) { return false; }
                    }
                }
            }
        }
        true
    }).collect()
}

// ── Analytics output types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct DashSummary {
    pub total_credit:    f64,
    pub total_debit:     f64,
    pub net_flow:        f64,
    pub opening_bal:     Option<f64>,
    pub closing_bal:     Option<f64>,
    pub txn_count:       usize,
    pub vendor_count:    usize,
    pub top_expense_head: String,
    pub top_expense_amt: f64,
}

#[derive(Debug, Clone)]
pub struct MonthlyAgg {
    pub labels:  Vec<String>,
    pub keys:    Vec<String>,  // "YYYY-MM", same order as labels/credits/debits
    pub credits: Vec<f64>,
    pub debits:  Vec<f64>,
}

/// Days in a given (1-indexed) month/year, accounting for leap years.
fn days_in_month(mm: u32, yyyy: i32) -> u32 {
    match mm {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if (yyyy % 4 == 0 && yyyy % 100 != 0) || yyyy % 400 == 0 { 29 } else { 28 },
        _ => 30,
    }
}

/// "YYYY-MM" -> (first day, last day) as "DD/MM/YYYY" strings.
pub fn month_key_to_range(key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() != 2 { return None; }
    let yyyy = parts[0].parse::<i32>().ok()?;
    let mm   = parts[1].parse::<u32>().ok()?;
    let last = days_in_month(mm, yyyy);
    Some((
        format!("01/{:02}/{:04}", mm, yyyy),
        format!("{:02}/{:02}/{:04}", last, mm, yyyy),
    ))
}

#[derive(Debug, Clone)]
pub struct ExpHead {
    pub label:  String,
    pub amount: f64,
    pub pct:    i32,
    pub color_idx: i32,
}

#[derive(Debug, Clone)]
pub struct CashPoint {
    pub norm: f32,   // 0..1 normalised
}

#[derive(Debug, Clone)]
pub struct VendorAgg {
    pub name:   String,
    pub debit:  f64,
    pub credit: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Insights {
    pub max_dr_amt:   String,
    pub max_dr_narr:  String,
    pub max_cr_amt:   String,
    pub max_cr_narr:  String,
    pub avg_dr:       String,
    pub avg_cr:       String,
    pub dr_count:     String,
    pub cr_count:     String,
    pub freq_vendor:  String,
}

pub struct AnalyticsResult {
    pub summary:  DashSummary,
    pub monthly:  MonthlyAgg,
    pub expenses: Vec<ExpHead>,
    pub cashflow: Vec<CashPoint>,
    pub vendors:  Vec<VendorAgg>,
    pub insights: Insights,
}

// ── Main compute ──────────────────────────────────────────────────────────────

pub fn compute(txns: &[Transaction], opening_bal: Option<f64>) -> AnalyticsResult {
    let real: Vec<&Transaction> = txns.iter()
        .filter(|t| !t.is_opening_balance)
        .collect();

    // ── Summary ───────────────────────────────────────────────────────────────
    let total_credit: f64 = real.iter().filter_map(|t| t.credit).sum();
    let total_debit:  f64 = real.iter().filter_map(|t| t.debit).sum();
    let last_bal = real.iter().rev().find_map(|t| t.balance);

    let mut vendor_set = std::collections::HashSet::new();
    for t in &real { if !t.vendor.is_empty() { vendor_set.insert(&t.vendor); } }

    let mut exp_by_head: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    for t in &real {
        if t.debit.is_some() && !t.account_head.is_empty() {
            *exp_by_head.entry(t.account_head.as_str()).or_default() += t.debit.unwrap_or(0.0);
        }
    }
    let mut exp_vec: Vec<(&str, f64)> = exp_by_head.into_iter().collect();
    exp_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top_exp = exp_vec.first().cloned().unwrap_or(("—", 0.0));

    let summary = DashSummary {
        total_credit,
        total_debit,
        net_flow: total_credit - total_debit,
        opening_bal,
        closing_bal: last_bal,
        txn_count:   real.len(),
        vendor_count: vendor_set.len(),
        top_expense_head: top_exp.0.to_string(),
        top_expense_amt:  top_exp.1,
    };

    // ── Monthly aggregation ───────────────────────────────────────────────────
    let mut month_map: std::collections::BTreeMap<String, (f64, f64)> =
        std::collections::BTreeMap::new();
    for t in &real {
        if let Some(mk) = month_key(&t.date) {
            let e = month_map.entry(mk).or_default();
            e.0 += t.credit.unwrap_or(0.0);
            e.1 += t.debit.unwrap_or(0.0);
        }
    }
    let max_monthly = month_map.values().map(|(c, d)| c.max(*d)).fold(0.0f64, f64::max);
    let monthly = MonthlyAgg {
        labels:  month_map.keys().map(|k| month_label(k)).collect(),
        keys:    month_map.keys().cloned().collect(),
        credits: month_map.values().map(|(c, _)| *c).collect(),
        debits:  month_map.values().map(|(_, d)| *d).collect(),
    };
    let _ = max_monthly; // used in normalisation below

    // ── Expense heads (top 10) ────────────────────────────────────────────────
    let exp_top10: Vec<(&str, f64)> = exp_vec.iter().take(10).cloned().collect();
    let exp_total: f64 = exp_top10.iter().map(|(_, v)| v).sum();
    let expenses: Vec<ExpHead> = exp_top10.iter().enumerate().map(|(i, (label, amt))| {
        let pct = if exp_total > 0.0 { (amt / exp_total * 100.0).round() as i32 } else { 0 };
        ExpHead {
            label: label.to_string(),
            amount: *amt,
            pct,
            color_idx: (i % 10) as i32,
        }
    }).collect();

    // ── Cash flow (balance over time, max 150 points) ─────────────────────────
    let bal_txns: Vec<f64> = real.iter()
        .filter_map(|t| t.balance)
        .collect();
    let step = if bal_txns.len() > 150 { bal_txns.len() / 150 } else { 1 };
    let sample: Vec<f64> = bal_txns.iter().step_by(step.max(1)).cloned().collect();
    let min_b = sample.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_b = sample.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = (max_b - min_b).max(1.0);
    let cashflow: Vec<CashPoint> = sample.iter().map(|b| CashPoint {
        norm: ((b - min_b) / range) as f32,
    }).collect();

    // ── Top vendors (by total amount, top 10) ─────────────────────────────────
    let mut vmap: std::collections::HashMap<&str, (f64, f64)> =
        std::collections::HashMap::new();
    for t in &real {
        if !t.vendor.is_empty() {
            let e = vmap.entry(t.vendor.as_str()).or_default();
            e.0 += t.debit.unwrap_or(0.0);
            e.1 += t.credit.unwrap_or(0.0);
        }
    }
    let mut vvec: Vec<(&str, f64, f64)> = vmap.iter()
        .map(|(n, (d, c))| (*n, *d, *c))
        .collect();
    vvec.sort_by(|a, b| (b.1 + b.2).partial_cmp(&(a.1 + a.2))
        .unwrap_or(std::cmp::Ordering::Equal));
    let vendors: Vec<VendorAgg> = vvec.iter().take(10).map(|(n, d, c)| VendorAgg {
        name: n.to_string(), debit: *d, credit: *c,
    }).collect();

    // ── Insights ──────────────────────────────────────────────────────────────
    let dr_txns: Vec<&&Transaction> = real.iter().filter(|t| t.debit.unwrap_or(0.0) > 0.0).collect();
    let cr_txns: Vec<&&Transaction> = real.iter().filter(|t| t.credit.unwrap_or(0.0) > 0.0).collect();

    let max_dr = dr_txns.iter().max_by(|a, b|
        a.debit.partial_cmp(&b.debit).unwrap_or(std::cmp::Ordering::Equal));
    let max_cr = cr_txns.iter().max_by(|a, b|
        a.credit.partial_cmp(&b.credit).unwrap_or(std::cmp::Ordering::Equal));

    let avg_dr = if dr_txns.is_empty() { 0.0 }
        else { total_debit / dr_txns.len() as f64 };
    let avg_cr = if cr_txns.is_empty() { 0.0 }
        else { total_credit / cr_txns.len() as f64 };

    // Most frequent vendor
    let mut freq: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in &real { if !t.vendor.is_empty() { *freq.entry(&t.vendor).or_default() += 1; } }
    let freq_vendor = freq.iter().max_by_key(|(_, c)| *c)
        .map(|(n, c)| format!("{}  ({}×)", n, c))
        .unwrap_or_else(|| "—".to_string());

    let insights = Insights {
        max_dr_amt:  max_dr.map(|t| fmt_inr(t.debit.unwrap_or(0.0))).unwrap_or("—".to_string()),
        max_dr_narr: max_dr.map(|t| t.narration.chars().take(45).collect()).unwrap_or_default(),
        max_cr_amt:  max_cr.map(|t| fmt_inr(t.credit.unwrap_or(0.0))).unwrap_or("—".to_string()),
        max_cr_narr: max_cr.map(|t| t.narration.chars().take(45).collect()).unwrap_or_default(),
        avg_dr:  if avg_dr > 0.0 { fmt_inr(avg_dr) } else { "—".to_string() },
        avg_cr:  if avg_cr > 0.0 { fmt_inr(avg_cr) } else { "—".to_string() },
        dr_count: if dr_txns.is_empty() { "—".to_string() } else { format!("{} debits", dr_txns.len()) },
        cr_count: if cr_txns.is_empty() { "—".to_string() } else { format!("{} credits", cr_txns.len()) },
        freq_vendor,
    };

    AnalyticsResult { summary, monthly, expenses, cashflow, vendors, insights }
}

// ── Unique lists for filter dropdowns ─────────────────────────────────────────

pub fn unique_banks(txns: &[Transaction]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for t in txns { if !t.is_opening_balance && !t.bank_name.is_empty() { set.insert(&t.bank_name); } }
    set.into_iter().map(|s| s.clone()).collect()
}

pub fn unique_vendors(txns: &[Transaction]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for t in txns { if !t.is_opening_balance && !t.vendor.is_empty() { set.insert(&t.vendor); } }
    set.into_iter().map(|s| s.clone()).collect()
}

pub fn unique_heads(txns: &[Transaction]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for t in txns { if !t.is_opening_balance && !t.account_head.is_empty() { set.insert(&t.account_head); } }
    set.into_iter().map(|s| s.clone()).collect()
}

// ── Convert to Slint model types ──────────────────────────────────────────────
// (called in main.rs with the generated Slint structs)

pub fn fmt_amt(v: Option<f64>) -> String {
    match v { None => "₹ —".to_string(), Some(n) => fmt_inr(n) }
}

pub fn fmt_short_pub(v: f64) -> String { fmt_short(v) }
