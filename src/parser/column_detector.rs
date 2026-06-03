//! Column detection — port of five functions from parser.js:
//!
//! | JS function              | Rust equivalent          |
//! |--------------------------|--------------------------|
//! | `_detectExcelCols(row)`  | `detect_excel_cols(row)` |
//! | `_findPDFHeader(rows)`   | `find_pdf_header(rows)`  |
//! | `_mergeAdjacentItems(r)` | `merge_adjacent_items`   |
//! | `_calcColBoundaries`     | `calc_col_boundaries`    |
//! | `_assignCells`           | `assign_cells`           |
//!
//! All scoring rules are ported exactly:
//!   exact match → 100  |  starts_with → 60  |  ends_with → 40
//!   contains (p≥4)→ 20 |  p.starts_with(val, val≥4) → 10
//!   minimum qualifying score = 10
//!
//! Assignment order (greedy, first claim wins):
//!   date → balance → debitcredit → debit → credit → narration → reference

use std::collections::HashMap;

use super::amount_parser::parse_amount_str;
use super::date_parser::is_valid_date_str;
use super::ColumnMap;

// ── Column keyword lists (exact port of Parser.COL) ─────────────────────────
//
// All patterns are lowercase.  Cells are lowercased before scoring so every
// comparison is case-insensitive by construction.

const COL_DATE: &[&str] = &[
    "date", "dt",
    "transaction date", "txn date", "tran date", "trans date",
    "value date", "value dt", "posting date", "booking date", "entry date",
    "transaction dt", "trans dt",
    "effective date", "process date",
];

const COL_NARRATION: &[&str] = &[
    "narration", "description", "particulars", "details", "remarks",
    "transaction description", "transaction remarks", "transaction narration",
    "transaction details", "tran description", "tran remarks",
    "transaction particulars",
    "transaction details/remarks",
    "tran particulars", "trans particulars", "chq/trn details",
    "trn description", "particulars of transaction", "payment details",
    "beneficiary details", "sender details",
];

const COL_REFERENCE: &[&str] = &[
    "reference", "ref no", "ref number", "ref.no.",
    "chq/ref no", "chq/ref. no.", "cheque / ref no", "cheque/ref no",
    "chq / ref number", "ref / chq no",
    "chq./ref.no.", "chq.ref.no.", "chq./ref no.", "chq. / ref. no.",
    "cheque no", "cheque no.", "cheque number", "chq no", "chq no.",
    "chq.no.", "chq. no.",
    "chq./txn. no.", "chq./txn.no.", "chq/txn no", "chq/txn no.",
    "txn no", "txn no.", "txn. no.", "txn number",
    "chq.",
    "transaction id", "utr", "utr no", "utr no.", "utr number",
    "instrument no", "instrument no.", "instrument", "instruments",
    "tran id", "chq / instrument no", "chq/instrument no",
    "ref.no", "ref no.", "ref num", "ref",
];

const COL_DEBIT: &[&str] = &[
    "debit", "dr", "dr.",
    "withdrawal", "withdrawal amt", "withdrawal amount", "withdrawal amt.",
    "withdrawals", "debit amount", "dr amount", "dr amt", "debit amt",
    "withdrawal amt.(inr)", "withdrawal amt. (inr)", "withdrawal amount (inr)",
    "withdrawal (dr)", "withdrawal (dr.)", "withdrawal(dr)", "withdrawal(dr.)",
    "debit (inr)", "debit(dr)",
    "paid out", "amount debit", "debit amount (inr)", "w/d",
];

const COL_CREDIT: &[&str] = &[
    "credit", "cr", "cr.",
    "deposit", "deposit amt", "deposit amount", "deposit amt.",
    "deposits", "credit amount", "cr amount", "cr amt", "credit amt",
    "deposit amt.(inr)", "deposit amt. (inr)", "deposit amount (inr)",
    "deposit (cr)", "deposit (cr.)", "deposit(cr)", "deposit(cr.)",
    "credit (inr)", "credit(cr)",
    "paid in", "amount credit", "credit amount (inr)",
];

const COL_BALANCE: &[&str] = &[
    "balance", "bal",
    "closing balance", "running balance", "closing bal",
    "available balance", "balance amt", "bal amt",
    "balance (inr)", "closing balance (inr)", "balance(inr)",
    "outstanding balance", "total balance", "ledger balance",
    "total amount", "total amt", "total bal", "running total",
    "closing amount", "book balance", "net balance",
];

const COL_DEBITCREDIT: &[&str] = &[
    "debit/credit", "debit / credit",
    "debit/credit(\u{20b9})", "debit / credit(\u{20b9})",
    "debit/credit (\u{20b9})", "debit / credit (\u{20b9})",
    "debit/credit(inr)", "debit / credit(inr)",
    "debit/credit (inr)", "debit / credit (inr)",
    "dr/cr", "dr / cr", "dr/cr(\u{20b9})", "dr / cr(\u{20b9})",
];
// \u{20b9} = ₹  (Indian Rupee Sign, U+20B9)

// Assignment order — must be exactly the same as JS ORDER constant.
// debitcredit before debit/credit so the combined "DEBIT/CREDIT(₹)" header
// wins its slot before the plain debit/credit keywords can steal it.
const ASSIGN_ORDER: &[ColField] = &[
    ColField::Date,
    ColField::Balance,
    ColField::DebitCredit,
    ColField::Debit,
    ColField::Credit,
    ColField::Narration,
    ColField::Reference,
];

// ── ColField enum ─────────────────────────────────────────────────────────────

/// Names the seven column slots shared by both Excel and PDF detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColField {
    Date,
    Narration,
    Reference,
    Debit,
    Credit,
    Balance,
    DebitCredit,
}

impl ColField {
    pub fn patterns(self) -> &'static [&'static str] {
        match self {
            ColField::Date        => COL_DATE,
            ColField::Narration   => COL_NARRATION,
            ColField::Reference   => COL_REFERENCE,
            ColField::Debit       => COL_DEBIT,
            ColField::Credit      => COL_CREDIT,
            ColField::Balance     => COL_BALANCE,
            ColField::DebitCredit => COL_DEBITCREDIT,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ColField::Date        => "date",
            ColField::Narration   => "narration",
            ColField::Reference   => "reference",
            ColField::Debit       => "debit",
            ColField::Credit      => "credit",
            ColField::Balance     => "balance",
            ColField::DebitCredit => "debitcredit",
        }
    }

    /// All seven fields in the canonical order used for iteration.
    pub fn all() -> &'static [ColField] {
        &[
            ColField::Date, ColField::Narration, ColField::Reference,
            ColField::Debit, ColField::Credit, ColField::Balance,
            ColField::DebitCredit,
        ]
    }
}

// ── PDF item ──────────────────────────────────────────────────────────────────

/// A single text item extracted from a PDF page.
/// Mirrors the `{ x, text, w }` objects used throughout parser.js PDF logic.
#[derive(Debug, Clone)]
pub struct PdfItem {
    /// X position in PDF points.
    pub x: f64,
    /// Extracted text content.
    pub text: String,
    /// Width of the text run in PDF points.
    pub w: f64,
}

// ── PDF column positions ──────────────────────────────────────────────────────

/// X-positions (PDF points) for each detected column.
/// `None` = column not detected.  Mirrors the dynamic `colX` object in JS.
#[derive(Debug, Clone, Default)]
pub struct PdfColX {
    pub date:         Option<f64>,
    pub narration:    Option<f64>,
    pub reference:    Option<f64>,
    pub debit:        Option<f64>,
    pub credit:       Option<f64>,
    pub balance:      Option<f64>,
    pub debit_credit: Option<f64>,
}

impl PdfColX {
    pub fn get(&self, f: ColField) -> Option<f64> {
        match f {
            ColField::Date        => self.date,
            ColField::Narration   => self.narration,
            ColField::Reference   => self.reference,
            ColField::Debit       => self.debit,
            ColField::Credit      => self.credit,
            ColField::Balance     => self.balance,
            ColField::DebitCredit => self.debit_credit,
        }
    }

    fn set(&mut self, f: ColField, x: f64) {
        match f {
            ColField::Date        => self.date        = Some(x),
            ColField::Narration   => self.narration   = Some(x),
            ColField::Reference   => self.reference   = Some(x),
            ColField::Debit       => self.debit       = Some(x),
            ColField::Credit      => self.credit      = Some(x),
            ColField::Balance     => self.balance     = Some(x),
            ColField::DebitCredit => self.debit_credit = Some(x),
        }
    }

    /// Iterator over (ColField, x) for all detected columns (Some values only).
    pub fn detected(&self) -> Vec<(ColField, f64)> {
        ColField::all().iter()
            .filter_map(|&f| self.get(f).map(|x| (f, x)))
            .collect()
    }

    /// True when the minimum viable set of columns is detected.
    pub fn is_usable(&self) -> bool {
        self.date.is_some()
            && self.narration.is_some()
            && (self.debit.is_some() || self.credit.is_some() || self.debit_credit.is_some())
    }
}

// ── PDF header result ─────────────────────────────────────────────────────────

pub struct PdfHeaderResult {
    /// 0-based index of the header row in the `rows` slice.
    /// `None` when the layout was inferred from data (no real text header row —
    /// corresponds to JS `hdrIdx === -1`).
    pub hdr_idx: Option<usize>,
    /// X-positions of each detected column.
    pub col_x: PdfColX,
    /// The original (non-merged) items from the header row.
    /// Empty when the header was inferred from data.
    pub hdr_row: Vec<PdfItem>,
}

// ── Column boundary ───────────────────────────────────────────────────────────

/// Half-open x-range `[x_min, x_max)` belonging to a detected column.
/// Produced by `calc_col_boundaries`; consumed by `assign_cells`.
#[derive(Debug, Clone)]
pub struct ColBoundary {
    pub field: ColField,
    /// Left edge (inclusive).  0.0 for the leftmost detected column.
    pub x_min: f64,
    /// Right edge (exclusive).  `f64::INFINITY` for the rightmost detected column.
    pub x_max: f64,
}

// ── Core scoring ──────────────────────────────────────────────────────────────

/// Normalise a cell text for scoring: lowercase → trim → collapse internal spaces.
/// Mirrors `String(cell || '').toLowerCase().trim().replace(/\s+/g, ' ')` from JS.
fn normalize_cell(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Score `val` (already normalised) against the pattern list for one field.
///
/// Scoring rules (exact copy of the JS loop):
/// ```text
/// val === p                           → 100  (break — can't improve)
/// val.starts_with(p)                  → max(best, 60)
/// else val.ends_with(p)               → max(best, 40)
/// else p.len()≥4 && val.contains(p)  → max(best, 20)
/// else val.len()≥4 && p.starts_with(val) → max(best, 10)
/// ```
/// Returns 0 if no pattern matches at all.
pub fn score_cell(val: &str, patterns: &[&str]) -> u32 {
    if val.is_empty() || val.len() < 2 {
        return 0;
    }
    let mut best = 0u32;
    for &p in patterns {
        if val == p {
            return 100;
        }
        if val.starts_with(p) {
            best = best.max(60);
        } else if val.ends_with(p) {
            best = best.max(40);
        } else if p.len() >= 4 && val.contains(p) {
            best = best.max(20);
        } else if val.len() >= 4 && p.starts_with(val) {
            best = best.max(10);
        }
    }
    best
}

// ── Excel column detector ─────────────────────────────────────────────────────

/// Port of `Parser._detectExcelCols(row)`.
///
/// Scores every cell in `row` against the seven column keyword lists, then
/// assigns the best-scoring column to each field in priority order
/// (date → balance → debitcredit → debit → credit → narration → reference).
///
/// Returns a [`ColumnMap`] with `-1` for fields that could not be detected.
/// A column qualifies only when its score is ≥ 10 (equivalent to `bestSc = 9`
/// then `sc > bestSc` in the JS).
///
/// Ties are resolved in favour of the lower column index — V8 iterates numeric
/// object keys in ascending order, so the leftmost equal-score column always wins.
pub fn detect_excel_cols<S: AsRef<str>>(row: &[S]) -> ColumnMap {
    // scores[field] = BTreeMap<col_index, score> (sorted by col for V8 parity)
    let mut scores: HashMap<ColField, std::collections::BTreeMap<usize, u32>> = HashMap::new();
    for &f in ColField::all() {
        scores.insert(f, std::collections::BTreeMap::new());
    }

    for (c, cell) in row.iter().enumerate() {
        let val = normalize_cell(cell.as_ref());
        if val.is_empty() || val.len() < 2 {
            continue;
        }
        for &field in ColField::all() {
            let sc = score_cell(&val, field.patterns());
            if sc > 0 {
                scores.get_mut(&field).unwrap().insert(c, sc);
            }
        }
    }

    let mut map = ColumnMap::default();
    let mut taken: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for &field in ASSIGN_ORDER {
        // Find best column for this field (minimum score to win = 10, i.e. > 9)
        let mut best_col = -1i32;
        let mut best_sc  = 9u32;

        // BTreeMap iterates in ascending key order → lower col index wins on tie
        for (&col, &sc) in &scores[&field] {
            if sc > best_sc && !taken.contains(&col) {
                best_sc  = sc;
                best_col = col as i32;
            }
        }

        if best_col >= 0 {
            let c = best_col as usize;
            match field {
                ColField::Date        => map.date         = best_col,
                ColField::Narration   => map.narration    = best_col,
                ColField::Reference   => map.reference    = best_col,
                ColField::Debit       => map.debit        = best_col,
                ColField::Credit      => map.credit       = best_col,
                ColField::Balance     => map.balance      = best_col,
                ColField::DebitCredit => map.debit_credit = best_col,
            }
            taken.insert(c);
        }
    }

    map
}

// ── Merge adjacent PDF items ──────────────────────────────────────────────────

/// Port of `Parser._mergeAdjacentItems(row, gapThreshold = 40)`.
///
/// Merges consecutive PDF text items whose inter-item gap is ≤ `gap_threshold`
/// points.  Gap = `next.x - (cur.x + cur.w)`.  When merging, the text is
/// joined with a space and the width is extended to cover both items.
pub fn merge_adjacent_items(row: &[PdfItem], gap_threshold: f64) -> Vec<PdfItem> {
    if row.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<PdfItem> = Vec::new();
    let mut cur = row[0].clone();

    for item in &row[1..] {
        let gap = item.x - (cur.x + cur.w);
        if gap <= gap_threshold {
            cur.text.push(' ');
            cur.text.push_str(&item.text);
            cur.w = (item.x + item.w) - cur.x;
        } else {
            out.push(cur);
            cur = item.clone();
        }
    }
    out.push(cur);
    out
}

// ── PDF header detector ───────────────────────────────────────────────────────

/// Port of `Parser._findPDFHeader(rows)`.
///
/// Scans the first 60 rows of a PDF page looking for a header row that
/// contains at minimum: date + narration + at least one amount column.
///
/// For each row both the original items and merged-adjacent items are scored.
/// The highest-scoring x-position wins each field (minimum score 10).
///
/// After assignment, detects split "DEBIT/CREDIT(₹)" labels where the bank
/// PDF renders the combined column title as two adjacent items ("DEBIT/" and
/// "CREDIT(₹)").  Both must be within 120 px and the debit item's text must
/// contain "/" (to distinguish from plain separate Debit + Credit columns).
///
/// Returns `None` when no valid header is found within the first 60 rows.
pub fn find_pdf_header(rows: &[Vec<PdfItem>]) -> Option<PdfHeaderResult> {
    for (i, row) in rows.iter().enumerate().take(60) {
        let merged   = merge_adjacent_items(row, 40.0);
        // allItems = [...row, ...merged]  (original first, merged second)
        let all_items: Vec<&PdfItem> = row.iter().chain(merged.iter()).collect();

        // For each field keep {x, score} of the best-scoring item so far.
        let mut scores: HashMap<ColField, (f64, u32)> = HashMap::new();

        for item in &all_items {
            let val = normalize_cell(&item.text);
            for &field in ColField::all() {
                let sc = score_cell(&val, field.patterns());
                if sc > 0 {
                    let prev_sc = scores.get(&field).map_or(0, |s| s.1);
                    if sc > prev_sc {
                        scores.insert(field, (item.x, sc));
                    }
                }
            }
        }

        // Assign fields in priority order (ties keep the first found since we
        // use strict `sc > prev_sc` above and original items come first).
        let mut col_x  = PdfColX::default();
        let mut taken_x: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for &field in ASSIGN_ORDER {
            if let Some(&(x, sc)) = scores.get(&field) {
                if sc >= 10 && !taken_x.contains(&x.to_bits()) {
                    col_x.set(field, x);
                    taken_x.insert(x.to_bits());
                }
            }
        }

        // ── Split "DEBIT/CREDIT(₹)" detection ────────────────────────────────
        // When both debit and credit are detected and are within 120 px of each
        // other, check if the raw header items confirm a compound label by
        // requiring the debit item's text to contain "/" (a plain "Debit" header
        // has no slash; the compound "DEBIT/" fragment does).
        if let (Some(dx), Some(cx)) = (col_x.debit, col_x.credit) {
            if (cx - dx).abs() < 120.0 {
                // Look for original (non-merged) items at those x positions
                let d_item = row.iter().find(|it| (it.x - dx).abs() < 15.0);
                let c_item = row.iter().find(|it| (it.x - cx).abs() < 15.0);

                let d_txt = d_item.map_or("", |it| it.text.as_str()).to_lowercase();
                let c_txt = c_item.map_or("", |it| it.text.as_str()).to_lowercase();

                let is_compound = d_txt.contains("debit")
                    && c_txt.contains("credit")
                    && !d_txt.contains("withdrawal")
                    && !d_txt.contains("deposit")
                    && !c_txt.contains("withdrawal")
                    && !c_txt.contains("deposit")
                    && d_txt.contains('/');

                if is_compound {
                    let dc_x = dx.min(cx);
                    col_x.debit_credit = Some(dc_x);
                    col_x.debit  = None;
                    col_x.credit = None;
                    log::debug!(
                        "[BSP Header] Split DEBIT/CREDIT merged → debitcredit x={}",
                        dc_x
                    );
                }
            }
        }

        // Valid header: date + narration + at least one amount column
        if col_x.date.is_some()
            && col_x.narration.is_some()
            && (col_x.debit.is_some()
                || col_x.credit.is_some()
                || col_x.debit_credit.is_some())
        {
            return Some(PdfHeaderResult {
                hdr_idx: Some(i),
                col_x,
                hdr_row: row.clone(),
            });
        }
    }

    None
}

// ── Column boundary calculator ────────────────────────────────────────────────

/// Port of `Parser._calcColBoundaries(colX, hdrRow = [])`.
///
/// For every detected column, computes the half-open x-range `[xMin, xMax)`
/// that "belongs" to it.  Boundaries are placed at the **midpoint** between
/// adjacent fence posts rather than between adjacent detected columns, so
/// unmapped columns in the header row act as walls that prevent bleed-over.
///
/// `hdr_row`:
/// - Non-empty → fence posts = all unique x-positions from the full header row
///   (includes unmapped columns like "#", "Type", "Channel", …).
/// - Empty     → fence posts = the detected column x-positions only.
///
/// Midpoints are rounded to the nearest integer (`Math.round`) to match JS.
/// The rightmost column's `x_max` is `f64::INFINITY`.
pub fn calc_col_boundaries(col_x: &PdfColX, hdr_row: &[PdfItem]) -> Vec<ColBoundary> {
    // Sort detected columns by x position (ascending)
    let mut sorted: Vec<(ColField, f64)> = col_x.detected();
    sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    if sorted.is_empty() {
        return Vec::new();
    }

    // Build fence list: all unique x-positions from hdr_row, or fall back to
    // mapped column x-positions when hdr_row is empty.
    let raw_fences: Vec<f64> = if !hdr_row.is_empty() {
        hdr_row.iter().map(|it| it.x).collect()
    } else {
        sorted.iter().map(|&(_, x)| x).collect()
    };

    // Deduplicate and sort (f64, so we can't use BTreeSet — use sort+dedup)
    let mut fences = raw_fences;
    fences.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    fences.dedup_by(|a, b| (*a - *b).abs() < 0.001);

    sorted
        .into_iter()
        .map(|(field, col_x_pos)| {
            // Left boundary: midpoint with nearest fence strictly LEFT of this column
            let x_min = fences
                .iter()
                .filter(|&&fx| fx < col_x_pos)
                .last()
                .map(|&left| ((left + col_x_pos) / 2.0).round())
                .unwrap_or(0.0);

            // Right boundary: midpoint with nearest fence strictly RIGHT of this column
            let x_max = fences
                .iter()
                .find(|&&fx| fx > col_x_pos)
                .map(|&right| ((col_x_pos + right) / 2.0).round())
                .unwrap_or(f64::INFINITY);

            ColBoundary { field, x_min, x_max }
        })
        .collect()
}

// ── Cell assigner ─────────────────────────────────────────────────────────────

/// Port of `Parser._assignCells(rowItems, boundaries)`.
///
/// Places each PDF item into the first matching boundary bucket, then resolves
/// each bucket to a single string value:
///
/// **Monetary columns** (debit/credit/balance/debitcredit) with >1 item:
/// - Try the space-joined string first (handles ICICI WM `"(-)" + "4,85,878.84"`
///   → `"(-) 4,85,878.84"` → parseable).
/// - Fall back to picking the **last** individually parseable amount — matches
///   the JS `for (const t of texts) { if parseable: best = t }` order which
///   overwrites `best` on each parseable item, leaving the last winner.
/// - If nothing parses individually, the joined string is the fallback.
///
/// **Date column** with >1 item:
/// - Take the **first** item that `is_valid_date_str` accepts.
/// - Fall back to the first item if none parse.
///
/// **Other columns**: join all items with a space.
pub fn assign_cells(
    row_items: &[PdfItem],
    boundaries: &[ColBoundary],
) -> HashMap<ColField, String> {
    const MONETARY: &[ColField] = &[
        ColField::Debit, ColField::Credit,
        ColField::Balance, ColField::DebitCredit,
    ];

    // Bucket items by field
    let mut buckets: HashMap<ColField, Vec<String>> = boundaries
        .iter()
        .map(|b| (b.field, Vec::new()))
        .collect();

    for item in row_items {
        for b in boundaries {
            if item.x >= b.x_min && item.x < b.x_max {
                buckets.get_mut(&b.field).unwrap().push(item.text.clone());
                break; // first matching boundary wins
            }
        }
    }

    // Resolve each bucket
    let mut result = HashMap::new();
    for b in boundaries {
        let texts = &buckets[&b.field];
        let value = if MONETARY.contains(&b.field) && texts.len() > 1 {
            // Try joined string first (ICICI WM "(-)" prefix case)
            let joined = texts.join(" ");
            if parse_amount_str(&joined).is_some() {
                joined
            } else {
                // Fall back: last individually parseable amount (BOM ref# + amount case)
                let mut best = joined; // fallback even if nothing individual parses
                for t in texts {
                    if parse_amount_str(t).is_some() {
                        best = t.clone();
                    }
                }
                best
            }
        } else if b.field == ColField::Date && texts.len() > 1 {
            // First item that parses as a valid date; else fall back to first item
            let valid = texts.iter().find(|t| is_valid_date_str(t.trim()));
            match valid {
                Some(s) => s.trim().to_owned(),
                None    => texts[0].trim().to_owned(),
            }
        } else {
            texts.join(" ").trim().to_owned()
        };

        result.insert(b.field, value);
    }

    result
}

// ── Content-based header inference ───────────────────────────────────────────

/// Port of `Parser._inferHeaderFromData(rows)`.
///
/// Used when `find_pdf_header` finds no text header (BOB, Union Bank, Mahanagar Co-op).
/// Scans the first 12 date-anchored rows among the first 40, buckets items by X
/// position (10-point resolution), and classifies each bucket as:
///   `date`   — item text matches `^\d{1,2}[\/\-]\d{1,2}[\/\-]\d{2,4}$`
///   `amt_cr` — item ends with "Cr" or "Dr" (BOB-style running balance)
///   `amount` — plain decimal with ≤ 10 integer digits
///   `ref_amt`— decimal with > 10 integer digits (UTR / reference number)
///   `text`   — everything else with length > 5
///
/// Returns `None` when < 2 date rows are found or usable column layout cannot be inferred.
/// `hdr_idx` is `None` in the result (no real header row).
pub fn infer_header_from_data(rows: &[Vec<PdfItem>]) -> Option<PdfHeaderResult> {
    // Regex-like checks using simple string operations for performance.
    let is_date_text = |t: &str| -> bool {
        let t = t.trim();
        if t.len() < 6 || t.len() > 10 { return false; }
        let parts: Vec<&str> = t.splitn(3, |c| c == '/' || c == '-').collect();
        if parts.len() != 3 { return false; }
        parts[0].chars().all(|c| c.is_ascii_digit()) &&
        parts[1].chars().all(|c| c.is_ascii_digit()) &&
        parts[2].chars().all(|c| c.is_ascii_digit()) &&
        parts[2].len() >= 2
    };

    let is_balcr = |t: &str| -> bool {
        let t = t.trim();
        let lower = t.to_lowercase();
        (lower.ends_with("cr") || lower.ends_with("dr")) &&
        t[..t.len()-2].trim_end().chars().last().map_or(false, |c| c.is_ascii_digit())
    };

    // Plain decimal: optional "Rs." prefix, optional minus, digits with commas, dot, 2 decimals.
    let is_amount = |t: &str| -> bool {
        let t = t.trim();
        let s = if t.to_lowercase().starts_with("rs") {
            t.trim_start_matches(|c: char| c.is_alphabetic() || c == '.')
              .trim_start()
        } else { t };
        let s = s.trim_start_matches('-');
        if s.is_empty() { return false; }
        let dot_pos = s.rfind('.');
        match dot_pos {
            None => false,
            Some(dp) => {
                let decimals = &s[dp+1..];
                let int_part = &s[..dp];
                decimals.chars().all(|c| c.is_ascii_digit()) &&
                decimals.len() >= 1 && decimals.len() <= 2 &&
                int_part.chars().all(|c| c.is_ascii_digit() || c == ',') &&
                !int_part.is_empty()
            }
        }
    };

    let int_digit_count = |t: &str| -> usize {
        // Number of digits before the decimal point, ignoring commas.
        let t = t.replace(',', "");
        let s = if let Some(dp) = t.find('.') { &t[..dp] } else { &t };
        s.chars().filter(|c| c.is_ascii_digit()).count()
    };

    // Collect up to 12 date-anchored rows from the first 40 rows.
    let mut data_rows: Vec<&Vec<PdfItem>> = Vec::new();
    for row in rows.iter().take(40) {
        if row.is_empty() { continue; }
        if is_date_text(row[0].text.trim()) {
            data_rows.push(row);
            if data_rows.len() >= 12 { break; }
        }
    }
    if data_rows.len() < 2 { return None; }

    // Bucket by X/10 resolution.
    #[derive(Default, Clone)]
    struct Bucket { date: u32, amt_cr: u32, amount: u32, ref_amt: u32, text: u32 }
    let mut buckets: std::collections::BTreeMap<i64, Bucket> = std::collections::BTreeMap::new();

    for row in &data_rows {
        for item in row.iter() {
            let t = item.text.trim();
            if t.is_empty() { continue; }
            let bx = (item.x / 10.0).round() as i64 * 10;
            let b = buckets.entry(bx).or_default();
            if is_date_text(t) {
                b.date += 1;
            } else if is_balcr(t) {
                b.amt_cr += 1;
            } else if is_amount(t) {
                if int_digit_count(t) <= 10 { b.amount += 1; }
                else                         { b.ref_amt += 1; }
            } else if t.len() > 5 {
                b.text += 1;
            }
        }
    }

    if buckets.len() < 2 { return None; }

    let entries: Vec<(f64, Bucket)> = buckets.into_iter()
        .map(|(x, b)| (x as f64, b))
        .collect(); // already sorted ascending by x (BTreeMap)

    let mut col_x = PdfColX::default();

    // Leftmost date cluster → date column.
    let date_cols: Vec<f64> = entries.iter()
        .filter(|(_, b)| b.date >= 2)
        .map(|(x, _)| *x)
        .collect();
    if date_cols.is_empty() { return None; }
    col_x.date = Some(date_cols[0]);

    // Rightmost Cr/Dr-suffixed amount → balance column (BOB, Cosmos).
    let balcr_cols: Vec<f64> = entries.iter()
        .filter(|(_, b)| b.amt_cr >= 1)
        .map(|(x, _)| *x)
        .collect();
    if !balcr_cols.is_empty() {
        col_x.balance = Some(*balcr_cols.last().unwrap());
    }

    // Plain-amount columns right of date, with no ref_amt mixing.
    let date_x = col_x.date.unwrap();
    let all_amt: Vec<f64> = entries.iter()
        .filter(|(x, b)| b.amount >= 1 && b.ref_amt == 0 && *x > date_x)
        .map(|(x, _)| *x)
        .collect();

    // If no Cr/Dr balance found, rightmost plain amount becomes balance.
    if col_x.balance.is_none() {
        if let Some(&bx) = all_amt.last() {
            col_x.balance = Some(bx);
        }
    }

    // Remaining amount columns (excl. balance) → debit / credit.
    let bal_x = col_x.balance;
    let txn_amt: Vec<f64> = all_amt.iter()
        .filter(|&&x| Some(x) != bal_x)
        .copied()
        .collect();

    match txn_amt.len() {
        n if n >= 2 => {
            col_x.debit  = Some(txn_amt[n - 2]);
            col_x.credit = Some(txn_amt[n - 1]);
        }
        1 => { col_x.debit = Some(txn_amt[0]); } // direction resolved later
        0 if col_x.balance.is_some() => return None, // all amounts = balance, can't split
        _ => {}
    }

    // Leftmost text cluster between date and first amount → narration.
    let nar_max_x = [col_x.debit, col_x.credit, col_x.balance]
        .iter().flatten().copied()
        .fold(f64::INFINITY, f64::min);

    let text_cols: Vec<f64> = entries.iter()
        .filter(|(x, b)| b.text >= 2 && *x > date_x && *x < nar_max_x)
        .map(|(x, _)| *x)
        .collect();
    if !text_cols.is_empty() {
        col_x.narration = Some(text_cols[0]);
    }

    // Reference/UTR column: large-digit amounts between narration and first txn amount.
    let ref_left  = col_x.narration.unwrap_or(date_x);
    let ref_right = [col_x.debit, col_x.credit, col_x.balance]
        .iter().flatten().copied()
        .fold(f64::INFINITY, f64::min);
    let ref_cols: Vec<f64> = entries.iter()
        .filter(|(x, b)| b.ref_amt >= 1 && *x > ref_left && *x < ref_right)
        .map(|(x, _)| *x)
        .collect();
    if !ref_cols.is_empty() {
        col_x.reference = Some(ref_cols[0]);
    }

    // Minimum viable: date + at least one amount.
    if col_x.date.is_none() ||
       (col_x.debit.is_none() && col_x.credit.is_none() && col_x.balance.is_none()) {
        return None;
    }

    Some(PdfHeaderResult {
        hdr_idx: None, // no real header row
        col_x,
        hdr_row: Vec::new(),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── score_cell ────────────────────────────────────────────────────────────

    #[test]
    fn score_exact_match() {
        assert_eq!(score_cell("date", COL_DATE), 100);
        assert_eq!(score_cell("balance", COL_BALANCE), 100);
        assert_eq!(score_cell("narration", COL_NARRATION), 100);
        assert_eq!(score_cell("debit/credit", COL_DEBITCREDIT), 100);
    }

    #[test]
    fn score_starts_with() {
        // "date of transaction" starts with "date" → 60
        assert_eq!(score_cell("date of transaction", COL_DATE), 60);
        // "balance carry" starts with "balance" → 60
        assert_eq!(score_cell("balance carry", COL_BALANCE), 60);
    }

    #[test]
    fn score_ends_with() {
        // "txn date" is exact in COL_DATE → 100 (not 40)
        assert_eq!(score_cell("txn date", COL_DATE), 100);
        // "account balance" ends with "balance" but doesn't start with any pattern → 40
        assert_eq!(score_cell("account balance", COL_BALANCE), 40);
    }

    #[test]
    fn score_contains() {
        // "current narration text" contains "narration" (len=9 ≥ 4) → 20
        // But doesn't start or end with any narration pattern
        assert_eq!(score_cell("current narration text", COL_NARRATION), 20);
    }

    #[test]
    fn score_p_starts_with_val() {
        // val = "with" (len=4 ≥ 4), pattern "withdrawal" starts with "with" → 10
        assert_eq!(score_cell("with", COL_DEBIT), 10);
    }

    #[test]
    fn score_short_val_is_zero() {
        assert_eq!(score_cell("a",  COL_DATE), 0);
        assert_eq!(score_cell("",   COL_DATE), 0);
    }

    #[test]
    fn score_no_match_is_zero() {
        assert_eq!(score_cell("xxxxxxxxxxx", COL_DATE), 0);
    }

    // ── normalize_cell ────────────────────────────────────────────────────────

    #[test]
    fn normalize_trims_and_collapses_spaces() {
        assert_eq!(normalize_cell("  Transaction  Date  "), "transaction date");
        assert_eq!(normalize_cell("DATE"), "date");
    }

    // ── detect_excel_cols ─────────────────────────────────────────────────────

    fn col(row: &[&str]) -> ColumnMap {
        detect_excel_cols(row)
    }

    // HDFC Bank header
    #[test]
    fn hdfc_header() {
        let row = &[
            "Date", "Narration", "Value Dt", "Chq/Ref No.",
            "Withdrawal Amt.", "Deposit Amt.", "Closing Balance",
        ];
        let m = col(row);
        assert_eq!(m.date,     0, "date col");
        assert_eq!(m.narration, 1, "narration col");
        assert_eq!(m.reference, 3, "reference col");
        assert_eq!(m.debit,    4, "debit col");
        assert_eq!(m.credit,   5, "credit col");
        assert_eq!(m.balance,  6, "balance col");
        assert_eq!(m.debit_credit, -1, "no debitcredit");
    }

    // SBI Bank header — two date-matching columns; leftmost (lower index) wins.
    //
    // PARITY NOTE: "Debit" (val="debit") scores 100 for the debit field AND 10 for
    // debitcredit (via `"debit/credit".starts_with("debit")` → p.starts_with(val) rule).
    // debitcredit has higher priority in ORDER → it claims col 4 before the debit field
    // can.  This is the exact JS _detectExcelCols behaviour.  The downstream
    // _correctDebitCreditByBalance post-pass then fixes the swap via balance movement.
    #[test]
    fn sbi_header() {
        let row = &[
            "Txn Date", "Value Date", "Description",
            "Ref No./Cheque No", "Debit", "Credit", "Balance",
        ];
        let m = col(row);
        assert_eq!(m.date,         0, "date col (Txn Date wins over Value Date)");
        assert_eq!(m.narration,    2, "narration col");
        assert_eq!(m.reference,    3, "reference col");
        // "Debit" → debitcredit scores 10 (p.starts_with(val)) > threshold 9 → claims col 4
        assert_eq!(m.debit_credit, 4, "debitcredit steals 'Debit' col (score 10 > 9)");
        assert_eq!(m.debit,       -1, "debit stolen by debitcredit");
        assert_eq!(m.credit,       5, "credit col ('Credit' scores 0 for debitcredit)");
        assert_eq!(m.balance,      6, "balance col");
    }

    // Axis Bank header — "PARTICULARS" maps to narration
    #[test]
    fn axis_header() {
        let row = &[
            "Tran Date", "PARTICULARS", "Chq./Ref.No.",
            "Withdrawal Amt.(INR)", "Deposit Amt.(INR)", "Balance (INR)",
        ];
        let m = col(row);
        assert_eq!(m.date,      0);
        assert_eq!(m.narration, 1);
        assert_eq!(m.reference, 2);
        assert_eq!(m.debit,     3);
        assert_eq!(m.credit,    4);
        assert_eq!(m.balance,   5);
    }

    // ICICI Bank header — "S No." at col 0 must NOT be assigned to any field
    #[test]
    fn icici_header() {
        let row = &[
            "S No.", "Transaction Date", "Value Date", "Transaction Remarks",
            "Ref No./Cheque No.", "Withdrawal Amt.(INR)", "Deposit Amt.(INR)", "Balance (INR)",
        ];
        let m = col(row);
        assert_eq!(m.date,      1, "Transaction Date → col 1");
        assert_eq!(m.narration, 3, "Transaction Remarks → col 3");
        assert_eq!(m.reference, 4, "Ref No./Cheque No. → col 4");
        assert_eq!(m.debit,     5);
        assert_eq!(m.credit,    6);
        assert_eq!(m.balance,   7);
    }

    // Kotak debitcredit column wins over plain debit/credit
    #[test]
    fn kotak_debitcredit_header() {
        let row = &["Date", "Narration", "Reference", "DEBIT/CREDIT(\u{20b9})", "Balance"];
        let m = col(row);
        assert_eq!(m.date,         0);
        assert_eq!(m.narration,    1);
        assert_eq!(m.reference,    2);
        assert_eq!(m.debit_credit, 3, "compound DEBIT/CREDIT(₹) → debitcredit col");
        assert_eq!(m.debit,  -1, "debit not assigned separately");
        assert_eq!(m.credit, -1, "credit not assigned separately");
        assert_eq!(m.balance, 4);
    }

    // Empty row → all -1
    #[test]
    fn empty_row() {
        let m = col(&["", "", "", ""]);
        assert_eq!(m.date,     -1);
        assert_eq!(m.narration, -1);
        assert_eq!(m.debit,    -1);
    }

    // Short single-character cells are ignored (length < 2)
    #[test]
    fn single_char_cells_ignored() {
        let m = col(&["D", "N", "R", "W", "C", "B"]);
        // "D" etc. are too short → score 0 → nothing mapped
        // except possibly "dr" if present — but we have "D" not "dr"
        assert_eq!(m.date, -1);
    }

    // Tie-breaking: lower column index wins when scores are equal for the same field.
    // Use headers that don't trigger the debitcredit steal (use "Withdrawal" / "Deposit"
    // which score only for debit/credit, not for debitcredit).
    #[test]
    fn tie_broken_by_lower_col_index() {
        // "Date" (col 0) and "Transaction Date" (col 1) both score 100 for date.
        // BTreeMap iterates ascending by col index → col 0 wins (strict sc > bestSc,
        // so col 1's equal score cannot displace col 0 once it is set).
        let m = col(&[
            "Date", "Transaction Date", "Narration",
            "Withdrawal Amt.", "Deposit Amt.", "Balance",
        ]);
        assert_eq!(m.date,      0, "col 0 'Date' wins tie with col 1 'Transaction Date'");
        assert_eq!(m.narration, 2);
        assert_eq!(m.debit,     3, "Withdrawal Amt. → debit");
        assert_eq!(m.credit,    4, "Deposit Amt. → credit");
        assert_eq!(m.balance,   5);
    }

    // ── merge_adjacent_items ──────────────────────────────────────────────────

    fn item(x: f64, text: &str, w: f64) -> PdfItem {
        PdfItem { x, text: text.to_owned(), w }
    }

    #[test]
    fn merge_empty_row() {
        assert_eq!(merge_adjacent_items(&[], 40.0).len(), 0);
    }

    #[test]
    fn merge_single_item() {
        let row = vec![item(10.0, "Date", 30.0)];
        let merged = merge_adjacent_items(&row, 40.0);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "Date");
    }

    #[test]
    fn merge_two_items_within_gap() {
        // item1 ends at x=10+30=40, item2 starts at x=45 → gap = 5 ≤ 40 → merge
        let row = vec![item(10.0, "DEBIT/", 30.0), item(45.0, "CREDIT(\u{20b9})", 60.0)];
        let merged = merge_adjacent_items(&row, 40.0);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "DEBIT/ CREDIT(\u{20b9})");
        assert_eq!(merged[0].x, 10.0);
        assert!((merged[0].w - 95.0).abs() < 0.001, "w = (45+60)-10 = 95");
    }

    #[test]
    fn merge_two_items_outside_gap() {
        // gap = 100 - (10+30) = 60 > 40 → separate
        let row = vec![item(10.0, "Date", 30.0), item(100.0, "Narration", 50.0)];
        let merged = merge_adjacent_items(&row, 40.0);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "Date");
        assert_eq!(merged[1].text, "Narration");
    }

    #[test]
    fn merge_three_items_first_two_close() {
        // gap(0→1) = 40-(10+25) = 5 ≤ 40 → merge; gap(merged→2) = 200-(40+25) = 135 > 40 → separate
        let row = vec![
            item(10.0,  "DEBIT/",       25.0),
            item(40.0,  "CREDIT(\u{20b9})", 25.0),
            item(200.0, "Balance",      40.0),
        ];
        let merged = merge_adjacent_items(&row, 40.0);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "DEBIT/ CREDIT(\u{20b9})");
        assert_eq!(merged[1].text, "Balance");
    }

    #[test]
    fn merge_custom_gap_threshold() {
        // gap = 60 > 40 (default) but ≤ 100 (custom)
        let row = vec![item(0.0, "A", 30.0), item(100.0, "B", 30.0)];
        let merged40  = merge_adjacent_items(&row, 40.0);
        let merged100 = merge_adjacent_items(&row, 100.0);
        assert_eq!(merged40.len(),  2, "gap 70 > 40 → separate");
        assert_eq!(merged100.len(), 1, "gap 70 ≤ 100 → merged");
    }

    // ── calc_col_boundaries ───────────────────────────────────────────────────

    fn col_x_from_vec(pairs: &[(ColField, f64)]) -> PdfColX {
        let mut cx = PdfColX::default();
        for &(f, x) in pairs {
            cx.set(f, x);
        }
        cx
    }

    fn bounds(pairs: &[(ColField, f64)], hdr: &[PdfItem]) -> Vec<ColBoundary> {
        calc_col_boundaries(&col_x_from_vec(pairs), hdr)
    }

    #[test]
    fn single_column_full_range() {
        let b = bounds(&[(ColField::Date, 50.0)], &[]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].x_min, 0.0, "leftmost → xMin = 0");
        assert_eq!(b[0].x_max, f64::INFINITY, "only col → xMax = ∞");
    }

    #[test]
    fn two_columns_midpoint() {
        // date at x=50, balance at x=150, hdrRow has both
        let hdr = vec![item(50.0, "Date", 30.0), item(150.0, "Balance", 40.0)];
        let b = bounds(
            &[(ColField::Date, 50.0), (ColField::Balance, 150.0)],
            &hdr,
        );
        // Sorted by x: date=50, balance=150
        // fences = [50, 150]
        // date:    leftFences=[], xMin=0; rightFences=[150], xMax=round((50+150)/2)=100
        // balance: leftFences=[50], xMin=round((50+150)/2)=100; rightFences=[], xMax=∞
        let date_b    = b.iter().find(|b| b.field == ColField::Date).unwrap();
        let balance_b = b.iter().find(|b| b.field == ColField::Balance).unwrap();
        assert_eq!(date_b.x_min,    0.0);
        assert_eq!(date_b.x_max,  100.0, "midpoint of 50 and 150");
        assert_eq!(balance_b.x_min, 100.0);
        assert_eq!(balance_b.x_max, f64::INFINITY);
    }

    #[test]
    fn unmapped_column_acts_as_fence() {
        // date=50, narration=150, balance=300.
        // hdrRow includes an unmapped item at x=100 (e.g. "#" col).
        let hdr = vec![
            item(50.0,  "Date",       30.0),
            item(100.0, "#",          10.0),  // unmapped — acts as fence
            item(150.0, "Narration",  60.0),
            item(300.0, "Balance",    40.0),
        ];
        let b = bounds(
            &[
                (ColField::Date,      50.0),
                (ColField::Narration, 150.0),
                (ColField::Balance,   300.0),
            ],
            &hdr,
        );
        // fences = [50, 100, 150, 300]
        // date (x=50):     leftFences=[], xMin=0; rightFences=[100,150,300], nearest=100, xMax=round((50+100)/2)=75
        // narration(x=150): leftFences=[50,100], nearest=100, xMin=round((100+150)/2)=125; rightFences=[300], xMax=round((150+300)/2)=225
        // balance (x=300): leftFences=[50,100,150], nearest=150, xMin=round((150+300)/2)=225; rightFences=[], xMax=∞
        let date_b = b.iter().find(|b| b.field == ColField::Date).unwrap();
        let narr_b = b.iter().find(|b| b.field == ColField::Narration).unwrap();
        let bal_b  = b.iter().find(|b| b.field == ColField::Balance).unwrap();
        assert_eq!(date_b.x_min,  0.0);
        assert_eq!(date_b.x_max, 75.0, "fence at 100 narrows date boundary");
        assert_eq!(narr_b.x_min, 125.0);
        assert_eq!(narr_b.x_max, 225.0);
        assert_eq!(bal_b.x_min,  225.0);
        assert_eq!(bal_b.x_max,  f64::INFINITY);
    }

    #[test]
    fn empty_hdr_row_falls_back_to_col_positions() {
        // When hdr_row is empty, fences = mapped col x-positions only
        let b = bounds(
            &[(ColField::Date, 50.0), (ColField::Balance, 200.0)],
            &[],
        );
        let date_b = b.iter().find(|b| b.field == ColField::Date).unwrap();
        // fences = [50, 200]; date midpoint = (50+200)/2 = 125
        assert_eq!(date_b.x_max, 125.0);
    }

    // ── assign_cells ─────────────────────────────────────────────────────────

    fn simple_bounds() -> Vec<ColBoundary> {
        vec![
            ColBoundary { field: ColField::Date,      x_min: 0.0,   x_max: 100.0 },
            ColBoundary { field: ColField::Narration,  x_min: 100.0, x_max: 200.0 },
            ColBoundary { field: ColField::Debit,      x_min: 200.0, x_max: 280.0 },
            ColBoundary { field: ColField::Credit,     x_min: 280.0, x_max: 360.0 },
            ColBoundary { field: ColField::Balance,    x_min: 360.0, x_max: f64::INFINITY },
        ]
    }

    #[test]
    fn assign_basic_row() {
        let items = vec![
            item(10.0,  "15/01/2024",  50.0),
            item(110.0, "NEFT PAYMENT",80.0),
            item(370.0, "100000.00",   60.0),
        ];
        let a = assign_cells(&items, &simple_bounds());
        assert_eq!(a[&ColField::Date],      "15/01/2024");
        assert_eq!(a[&ColField::Narration], "NEFT PAYMENT");
        assert_eq!(a[&ColField::Balance],   "100000.00");
        assert_eq!(a.get(&ColField::Debit).map(|s| s.as_str()), Some(""));
        assert_eq!(a.get(&ColField::Credit).map(|s| s.as_str()), Some(""));
    }

    // ICICI WM: "(-)" and "4,85,878.84" in same monetary bucket → join wins
    #[test]
    fn assign_monetary_joined_wins_for_icici_wm() {
        let bounds = vec![
            ColBoundary { field: ColField::Debit, x_min: 0.0, x_max: f64::INFINITY },
        ];
        let items = vec![
            item(10.0, "(-)",        20.0),
            item(35.0, "4,85,878.84", 60.0),
        ];
        let a = assign_cells(&items, &bounds);
        assert_eq!(a[&ColField::Debit], "(-) 4,85,878.84",
            "joined \"(-) 4,85,878.84\" parses as amount → use joined");
    }

    // BOM: ref# + amount in same bucket → joined fails → last individual parseable wins
    #[test]
    fn assign_monetary_last_parseable_wins_for_bom() {
        let bounds = vec![
            ColBoundary { field: ColField::Balance, x_min: 0.0, x_max: f64::INFINITY },
        ];
        // "303213675227" has 12 digits → parse_amount_str returns None
        // "22000.00" parses → wins as last parseable
        let items = vec![
            item(10.0, "303213675227", 60.0),
            item(80.0, "22000.00",     50.0),
        ];
        let a = assign_cells(&items, &bounds);
        assert_eq!(a[&ColField::Balance], "22000.00",
            "joined fails (12-digit ref); last individual parseable wins");
    }

    // Date bucket: "Cheque" lands in date zone alongside the real date → pick date
    #[test]
    fn assign_date_picks_first_valid() {
        let bounds = vec![
            ColBoundary { field: ColField::Date, x_min: 0.0, x_max: f64::INFINITY },
        ];
        let items = vec![
            item(10.0, "15/01/2024", 50.0),
            item(60.0, "Cheque",     40.0),
        ];
        let a = assign_cells(&items, &bounds);
        assert_eq!(a[&ColField::Date], "15/01/2024",
            "first valid date picked over trailing 'Cheque' text");
    }

    // Date bucket: no valid date → fall back to first item
    #[test]
    fn assign_date_fallback_to_first_item() {
        let bounds = vec![
            ColBoundary { field: ColField::Date, x_min: 0.0, x_max: f64::INFINITY },
        ];
        let items = vec![
            item(10.0, "Cheque", 40.0),
            item(60.0, "123",    20.0),
        ];
        let a = assign_cells(&items, &bounds);
        assert_eq!(a[&ColField::Date], "Cheque",
            "no valid date → fall back to first item");
    }

    // Single monetary item → returned as-is (no multi-item logic)
    #[test]
    fn assign_single_monetary_item_as_is() {
        let bounds = vec![
            ColBoundary { field: ColField::Credit, x_min: 0.0, x_max: f64::INFINITY },
        ];
        let items = vec![item(10.0, "50,000.00", 60.0)];
        let a = assign_cells(&items, &bounds);
        assert_eq!(a[&ColField::Credit], "50,000.00");
    }

    // Non-monetary multi-item → joined with space
    #[test]
    fn assign_narration_multi_item_joined() {
        let bounds = vec![
            ColBoundary { field: ColField::Narration, x_min: 0.0, x_max: f64::INFINITY },
        ];
        let items = vec![
            item(10.0, "NEFT", 25.0),
            item(40.0, "PAYMENT FROM RAM", 80.0),
        ];
        let a = assign_cells(&items, &bounds);
        assert_eq!(a[&ColField::Narration], "NEFT PAYMENT FROM RAM");
    }

    // Item outside all boundaries → not assigned
    #[test]
    fn assign_item_outside_boundaries_ignored() {
        let bounds = vec![
            ColBoundary { field: ColField::Date, x_min: 0.0, x_max: 50.0 },
        ];
        let items = vec![item(100.0, "orphan", 20.0)]; // x=100 ≥ 50 = x_max → not in date
        let a = assign_cells(&items, &bounds);
        // "orphan" doesn't land in the date bucket
        assert_eq!(a[&ColField::Date], "");
    }

    // ── find_pdf_header ───────────────────────────────────────────────────────

    fn make_row(items: &[(f64, &str, f64)]) -> Vec<PdfItem> {
        items.iter().map(|&(x, t, w)| item(x, t, w)).collect()
    }

    #[test]
    fn find_pdf_header_simple_row() {
        let rows = vec![
            make_row(&[(10.0, "Date", 30.0), (100.0, "Narration", 60.0), (300.0, "Credit", 40.0)]),
        ];
        let result = find_pdf_header(&rows).expect("should find header");
        assert_eq!(result.hdr_idx, Some(0));
        assert!(result.col_x.date.is_some());
        assert!(result.col_x.narration.is_some());
        assert!(result.col_x.credit.is_some());
    }

    #[test]
    fn find_pdf_header_skips_non_header_rows() {
        let rows = vec![
            // Row 0: no recognisable header keywords
            make_row(&[(10.0, "01/01/2024", 40.0), (100.0, "Some narration", 80.0)]),
            // Row 1: valid header — needs date + narration + at least one of debit/credit/debitcredit
            // (balance alone is not sufficient per the JS validity check)
            make_row(&[
                (10.0,  "Date",      30.0),
                (100.0, "Narration", 60.0),
                (300.0, "Credit",    40.0),
            ]),
        ];
        let result = find_pdf_header(&rows).expect("should find header");
        assert_eq!(result.hdr_idx, Some(1), "header at row 1, not row 0");
    }

    #[test]
    fn find_pdf_header_returns_none_when_no_header() {
        let rows = vec![
            make_row(&[(10.0, "01/01/2024", 40.0), (100.0, "Some narration", 80.0)]),
        ];
        assert!(find_pdf_header(&rows).is_none());
    }

    #[test]
    fn find_pdf_header_requires_date_narration_and_amount() {
        // date + debit but no narration → should NOT match
        let rows = vec![
            make_row(&[(10.0, "Date", 30.0), (300.0, "Debit", 40.0)]),
        ];
        assert!(find_pdf_header(&rows).is_none(), "narration required");
    }

    #[test]
    fn find_pdf_header_merges_split_debitcredit() {
        // "DEBIT/" and "CREDIT(₹)" close together (gap = 40-30 = 10 ≤ 40) →
        // after scoring the merged item, debit_credit should be detected
        let rows = vec![make_row(&[
            (10.0,  "Date",                30.0),
            (100.0, "Narration",           60.0),
            (200.0, "DEBIT/",             25.0),  // split label part 1
            (230.0, "CREDIT(\u{20b9})",   40.0),  // split label part 2 (gap = 230-225 = 5)
        ])];
        let result = find_pdf_header(&rows).expect("header found");
        // The merged item "DEBIT/ CREDIT(₹)" scores for debitcredit
        assert!(result.col_x.debit_credit.is_some(), "debitcredit detected via merged item");
    }

    // The slash guard fires when "DEBIT/(₹)" (with "/" and rupee suffix) and
    // "CREDIT(₹)" are both detected as separate debit/credit positions.
    // "DEBIT/(₹)" does NOT score for debitcredit (the rupee suffix breaks all
    // p.starts_with(val) matches) so both positions survive the assignment step.
    // The guard then detects the compound label via the "/" in the debit text and
    // merges both into a single debitcredit position.
    #[test]
    fn find_pdf_header_slash_guard_merges_compound_label() {
        let rows = vec![make_row(&[
            (10.0,  "Date",                   30.0),
            (100.0, "Narration",              60.0),
            (200.0, "DEBIT/(\u{20b9})",       30.0),  // "/" + rupee: scores debit=60, debitcredit=0
            (230.0, "CREDIT(\u{20b9})",       30.0),  // within 120px; scores credit=60
        ])];
        let result = find_pdf_header(&rows).expect("header found");
        assert!(result.col_x.debit_credit.is_some(),
            "compound 'DEBIT/(₹)'+'CREDIT(₹)' merged into debitcredit");
        assert!(result.col_x.debit.is_none(),
            "debit cleared after compound merge");
        assert!(result.col_x.credit.is_none(),
            "credit cleared after compound merge");
    }

    // Plain "Debit" (no slash) scores 10 for debitcredit via p.starts_with(val).
    // debitcredit has higher priority in ORDER → steals the column.
    // col_x.debit = None → guard condition `debit && credit` is not met → no merge.
    // This is correct JS behaviour: the slash guard requires "/" in the debit item text.
    #[test]
    fn find_pdf_header_no_slash_no_merge() {
        let rows = vec![make_row(&[
            (10.0,  "Date",      30.0),
            (100.0, "Narration", 60.0),
            (200.0, "Debit",     30.0),  // no slash; debitcredit steals it (score 10 > 9)
            (240.0, "Credit",    30.0),
        ])];
        let result = find_pdf_header(&rows).expect("header found");
        // "Debit" (score 10 for debitcredit) → debitcredit claims x=200 first
        assert!(result.col_x.debit_credit.is_some(),
            "debitcredit claims 'Debit' x-pos (score 10 > threshold 9)");
        assert!(result.col_x.debit.is_none(),
            "debit not separately set — stolen by debitcredit");
        // "Credit" scores 0 for debitcredit → not stolen; remains as credit
        assert!(result.col_x.credit.is_some(),
            "credit separately detected ('Credit' scores 0 for debitcredit)");
    }

    // ── infer_header_from_data ────────────────────────────────────────────────

    fn data_item(x: f64, text: &str) -> PdfItem {
        PdfItem { x, text: text.to_owned(), w: 30.0 }
    }

    fn data_row(pairs: &[(f64, &str)]) -> Vec<PdfItem> {
        pairs.iter().map(|&(x, t)| data_item(x, t)).collect()
    }

    // Standard 5-column layout: date | narration | debit | credit | balance
    // Simulates BOB/Union Bank (no text header, dates start each data row).
    fn standard_data_rows() -> Vec<Vec<PdfItem>> {
        // Build 5 rows, each starting with a date at x≈10
        let transactions = [
            ("01/01/2024", "SALARY CREDIT",   "",          "50000.00", "1,50,000.00"),
            ("02/01/2024", "ATM WDL BANDRA",  "10000.00",  "",         "1,40,000.00"),
            ("03/01/2024", "SWIGGY ORDER",     "850.00",    "",         "1,39,150.00"),
            ("04/01/2024", "NEFT FROM RAJESH", "",          "25000.00", "1,64,150.00"),
            ("05/01/2024", "BPCL PETROL",      "3500.00",   "",         "1,60,650.00"),
        ];
        transactions.iter().map(|&(date, narr, dr, cr, bal)| {
            let mut row = vec![
                data_item(10.0,  date),
                data_item(100.0, narr),
            ];
            if !dr.is_empty()  { row.push(data_item(280.0, dr)); }
            if !cr.is_empty()  { row.push(data_item(360.0, cr)); }
            row.push(data_item(440.0, bal));
            row
        }).collect()
    }

    #[test]
    fn infer_detects_date_column() {
        let rows = standard_data_rows();
        let result = infer_header_from_data(&rows).expect("should infer");
        assert!(result.col_x.date.is_some(), "date column inferred");
        // date bucket ≈ x=10
        assert!((result.col_x.date.unwrap() - 10.0).abs() < 5.0,
            "date x ≈ 10, got {}", result.col_x.date.unwrap());
    }

    #[test]
    fn infer_detects_amount_columns() {
        let rows = standard_data_rows();
        let result = infer_header_from_data(&rows).expect("should infer");
        // Should detect at least one of debit/credit/balance
        let has_amount = result.col_x.debit.is_some()
            || result.col_x.credit.is_some()
            || result.col_x.balance.is_some();
        assert!(has_amount, "at least one amount column inferred");
    }

    #[test]
    fn infer_hdr_idx_is_none() {
        let rows = standard_data_rows();
        let result = infer_header_from_data(&rows).expect("should infer");
        assert!(result.hdr_idx.is_none(), "no real header → hdr_idx = None");
        assert!(result.hdr_row.is_empty(), "hdr_row empty for inferred layout");
    }

    #[test]
    fn infer_requires_two_date_rows() {
        // Only one date row → returns None
        let rows = vec![
            data_row(&[(10.0, "01/01/2024"), (100.0, "SALARY"), (300.0, "50000.00")]),
        ];
        assert!(infer_header_from_data(&rows).is_none(), "need ≥ 2 date rows");
    }

    #[test]
    fn infer_detects_cr_dr_balance() {
        // BOB-style: balance column uses "22,95,856.02Cr" suffix
        let rows: Vec<Vec<PdfItem>> = (0..3).map(|i| {
            vec![
                data_item(10.0,  &format!("0{}/01/2024", i+1)),
                data_item(100.0, "NARRATION TEXT HERE"),
                data_item(280.0, "5000.00"),
                data_item(400.0, &format!("{},000.00Cr", 95 + i)),
            ]
        }).collect();
        let result = infer_header_from_data(&rows).expect("should infer");
        assert!(result.col_x.balance.is_some(), "Cr-suffix column → balance");
        assert!((result.col_x.balance.unwrap() - 400.0).abs() < 5.0);
    }

    #[test]
    fn infer_utr_column_not_treated_as_amount() {
        // UTR numbers (>10 int digits) should NOT be classified as debit/credit.
        let rows: Vec<Vec<PdfItem>> = (0..3).map(|i| {
            vec![
                data_item(10.0,  &format!("0{}/01/2024", i+1)),
                data_item(100.0, "NEFT PAYMENT"),
                data_item(200.0, "303213675227.00"), // 12 int digits → refAmt
                data_item(300.0, "5000.00"),
                data_item(400.0, "95000.00"),
            ]
        }).collect();
        let result = infer_header_from_data(&rows).expect("should infer");
        // x=200 (UTR) should not be assigned to debit or credit
        let utr_x = 200.0;
        let assigned_utr = [result.col_x.debit, result.col_x.credit]
            .iter().flatten()
            .any(|&x| (x - utr_x).abs() < 5.0);
        assert!(!assigned_utr, "UTR/ref column x=200 must not be debit or credit");
    }
}
