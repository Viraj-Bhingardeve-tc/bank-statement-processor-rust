//! row_builder.rs — Port of `Parser._clusterIntoRows(items)`.
//!
//! Accepts items that carry a Y coordinate (as extracted from a PDF page),
//! groups them into rows by Y-proximity, then discards Y so every downstream
//! function receives the standard `Vec<Vec<PdfItem>>` type.
//!
//! Y coordinates are only meaningful here; everything downstream uses X only.

use crate::parser::column_detector::PdfItem;

// ── RawPdfItem ────────────────────────────────────────────────────────────────

/// A PDF text item as extracted from the page, before row-clustering.
/// Carries the Y coordinate that is discarded after `cluster_into_rows`.
///
/// Mirrors the `{ text, x, y, w }` objects produced by the pdf.js
/// `page.getTextContent()` call (with the Y-flip applied so origin = top-left).
#[derive(Debug, Clone)]
pub struct RawPdfItem {
    pub text: String,
    /// X position in PDF points (rounded to nearest integer, as in JS).
    pub x: f64,
    /// Y position in PDF points, **top-left origin** (Y-flipped from PDF standard).
    pub y: f64,
    /// Width of the text run in PDF points.
    pub w: f64,
}

impl RawPdfItem {
    pub fn new(text: impl Into<String>, x: f64, y: f64, w: f64) -> Self {
        RawPdfItem { text: text.into(), x, y, w }
    }

    /// Convert to a `PdfItem` (drops Y).
    fn into_pdf_item(self) -> PdfItem {
        PdfItem { x: self.x, text: self.text, w: self.w }
    }
}

// ── cluster_into_rows ─────────────────────────────────────────────────────────

/// Port of `Parser._clusterIntoRows(items)`.
///
/// Groups items whose Y coordinates differ by ≤ `y_tolerance` points into
/// the same row, then sorts each row by X (left-to-right).
///
/// The JS default tolerance is 5 pixels; callers should pass `5.0`.
///
/// Steps:
///   1. Sort all items by (Y asc, X asc).
///   2. Walk items: if |item.y − curY| ≤ tolerance → append to current row.
///   3. Otherwise flush current row (re-sort by X), start a new row.
///   4. Flush the last row.
pub fn cluster_into_rows(mut items: Vec<RawPdfItem>, y_tolerance: f64) -> Vec<Vec<PdfItem>> {
    if items.is_empty() {
        return Vec::new();
    }

    // Sort by Y ascending, then X ascending (mirrors JS sort: (a,b) => a.y-b.y || a.x-b.x)
    items.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut rows: Vec<Vec<PdfItem>> = Vec::new();
    let mut cur_y = items[0].y;
    let mut cur_row: Vec<PdfItem> = Vec::new();

    for item in items {
        if (item.y - cur_y).abs() <= y_tolerance {
            cur_row.push(item.into_pdf_item());
        } else {
            if !cur_row.is_empty() {
                // Re-sort by X within the row (JS: curRow.sort((a, b) => a.x - b.x))
                cur_row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
                rows.push(cur_row);
            }
            cur_y = item.y;
            cur_row = vec![item.into_pdf_item()];
        }
    }
    if !cur_row.is_empty() {
        cur_row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        rows.push(cur_row);
    }

    rows
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(x: f64, y: f64, text: &str) -> RawPdfItem {
        RawPdfItem::new(text, x, y, 30.0)
    }

    // ── Basic clustering ──────────────────────────────────────────────────────

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(cluster_into_rows(vec![], 5.0).len(), 0);
    }

    #[test]
    fn single_item_one_row() {
        let rows = cluster_into_rows(vec![raw(10.0, 100.0, "Date")], 5.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].text, "Date");
    }

    #[test]
    fn two_items_same_y_one_row() {
        let items = vec![raw(100.0, 50.0, "Narration"), raw(10.0, 50.0, "Date")];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2);
    }

    #[test]
    fn items_within_tolerance_grouped() {
        // Y=10 and Y=14 differ by 4 ≤ 5 → same row
        let items = vec![raw(10.0, 10.0, "A"), raw(50.0, 14.0, "B")];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 1, "Y diff = 4 ≤ 5 → same row");
    }

    #[test]
    fn items_outside_tolerance_separate_rows() {
        // Y=10 and Y=16 differ by 6 > 5 → different rows
        let items = vec![raw(10.0, 10.0, "Row1"), raw(50.0, 16.0, "Row2")];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 2, "Y diff = 6 > 5 → separate rows");
    }

    #[test]
    fn exactly_at_tolerance_boundary_grouped() {
        // |14 - 10| = 4 ≤ 5 (the JS condition is `<= 5`, not `< 5`)
        let items = vec![raw(10.0, 10.0, "A"), raw(50.0, 15.0, "B")];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 1, "Y diff = 5 ≤ 5 → same row");
    }

    #[test]
    fn just_outside_tolerance_separate() {
        let items = vec![raw(10.0, 10.0, "A"), raw(50.0, 15.01, "B")];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 2, "Y diff = 5.01 > 5 → separate");
    }

    // ── Sorting ───────────────────────────────────────────────────────────────

    #[test]
    fn items_sorted_by_x_within_row() {
        // Items given out of X order; after clustering they should be sorted by X
        let items = vec![
            raw(300.0, 50.0, "Balance"),
            raw(10.0,  50.0, "Date"),
            raw(150.0, 50.0, "Narration"),
        ];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows[0][0].text, "Date",      "leftmost item first");
        assert_eq!(rows[0][1].text, "Narration");
        assert_eq!(rows[0][2].text, "Balance",   "rightmost item last");
    }

    #[test]
    fn rows_sorted_by_y_ascending() {
        // Items given in reverse Y order; rows should be top-to-bottom
        let items = vec![
            raw(10.0, 200.0, "Row3"),
            raw(10.0, 100.0, "Row1"),
            raw(10.0, 150.0, "Row2"),
        ];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0].text, "Row1", "lowest Y (topmost) first");
        assert_eq!(rows[1][0].text, "Row2");
        assert_eq!(rows[2][0].text, "Row3");
    }

    // ── Multi-row realistic scenario ──────────────────────────────────────────

    #[test]
    fn hdfc_style_three_rows() {
        // Simulate a 3-row PDF section: header + 2 transactions
        let items = vec![
            // Row 0 (Y≈10): header items
            raw(10.0,  10.0, "Date"),
            raw(100.0, 10.0, "Narration"),
            raw(300.0, 12.0, "Balance"),   // slight Y variation, still same row
            // Row 1 (Y≈50): first transaction
            raw(10.0,  50.0, "01/01/2024"),
            raw(100.0, 51.0, "SALARY CREDIT"),
            raw(300.0, 50.0, "85000.00"),
            // Row 2 (Y≈90): second transaction
            raw(10.0,  90.0, "02/01/2024"),
            raw(100.0, 90.0, "ATM WDL"),
            raw(300.0, 91.0, "75000.00"),
        ];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].len(), 3, "header row: 3 items");
        assert_eq!(rows[1].len(), 3, "txn row 1: 3 items");
        assert_eq!(rows[2].len(), 3, "txn row 2: 3 items");
        // Verify left-to-right order within each row
        assert_eq!(rows[0][0].text, "Date");
        assert_eq!(rows[1][0].text, "01/01/2024");
    }

    // ── Y is discarded after clustering ──────────────────────────────────────

    #[test]
    fn output_items_have_x_and_text_not_y() {
        let items = vec![raw(42.0, 100.0, "Test")];
        let rows = cluster_into_rows(items, 5.0);
        // PdfItem has x and text; no y field
        assert!((rows[0][0].x - 42.0).abs() < 0.001);
        assert_eq!(rows[0][0].text, "Test");
    }
}
