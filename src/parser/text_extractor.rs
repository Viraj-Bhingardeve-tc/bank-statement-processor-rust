//! text_extractor.rs — PDF text extraction using lopdf.
//!
//! lopdf does NOT provide X/Y glyph positions.  Each page's text is returned
//! as a flat string in reading order.  This module builds two views of that text:
//!
//! 1. `extract_pages` — returns `Vec<Vec<PdfItem>>` where every line is ONE item
//!    placed at X=0.  This makes every PDF look like a fixed-width layout so that
//!    `pdf_parser::is_fw_format` returns true and `extract_fw_transactions` is tried.
//!
//! 2. `extract_full_text` — returns the raw concatenated text for `ocr_parser::parse_ocr_text`.
//!
//! **Bug fixed**: the original code passed `page_id.0` (a lopdf *object ID*, e.g. 5, 12, 38)
//! to `extract_text(&[page_id.0])` as if it were a *page number* (1, 2, 3…).
//! `doc.get_pages()` returns `BTreeMap<u32, ObjectId>` where keys ARE page numbers.

use std::path::Path;

use anyhow::Result;

use crate::parser::{
    column_detector::PdfItem,
    row_builder::{cluster_into_rows, RawPdfItem},
};

// ── extract_pages ─────────────────────────────────────────────────────────────

/// Extract text from a PDF and cluster it into rows.
///
/// Each non-empty line of text becomes **one `PdfItem` at X = 0**.
/// Placing every item at X = 0 makes `is_fw_format` return true, allowing
/// `extract_fw_transactions` to attempt character-position parsing.
///
/// Page numbers are taken from `doc.get_pages()` (1-indexed keys) — NOT from
/// the object IDs returned by `page_iter()`.
pub fn extract_pages(path: &Path) -> Result<Vec<Vec<PdfItem>>> {
    let doc = lopdf::Document::load(path)?;
    if doc.is_encrypted() {
        anyhow::bail!("PDF is password-protected");
    }

    // get_pages() → BTreeMap<page_number (1-based), ObjectId>
    let pages = doc.get_pages();
    if pages.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_raw: Vec<RawPdfItem> = Vec::new();

    for page_num in pages.keys() {
        // extract_text expects 1-based page numbers — use the BTreeMap key directly.
        let page_text = doc.extract_text(&[*page_num]).unwrap_or_default();

        // Y offset separates pages so their lines don't cluster together.
        let y_offset = (*page_num as f64 - 1.0) * 10_000.0;

        for (line_idx, line) in page_text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // One item per line, at X = 0 — full line text preserved.
            // This enables both FW-format and OCR-text parsing downstream.
            let y = y_offset + (line_idx as f64 * 15.0);
            all_raw.push(RawPdfItem::new(line, 0.0, y, (line.len() as f64) * 6.0));
        }
    }

    log::debug!(
        "[TextExtractor] {} pages → {} raw lines before clustering",
        pages.len(),
        all_raw.len()
    );

    Ok(cluster_into_rows(all_raw, 5.0))
}

// ── extract_full_text ─────────────────────────────────────────────────────────

/// Return the entire PDF as one string (pages joined with newlines).
/// Used as input to `ocr_parser::parse_ocr_text`.
///
/// Also uses correct 1-based page numbers from `doc.get_pages()`.
pub fn extract_full_text(path: &Path) -> String {
    let doc = match lopdf::Document::load(path) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[TextExtractor] load failed: {}", e);
            return String::new();
        }
    };

    let pages = doc.get_pages();
    let mut parts = Vec::with_capacity(pages.len());

    for page_num in pages.keys() {
        match doc.extract_text(&[*page_num]) {
            Ok(t) => parts.push(t),
            Err(e) => log::debug!("[TextExtractor] page {} extract error: {}", page_num, e),
        }
    }

    let full = parts.join("\n");
    log::debug!(
        "[TextExtractor] full_text: {} pages, {} chars",
        pages.len(),
        full.len()
    );
    full
}

// ── Password-aware extraction ─────────────────────────────────────────────────

pub fn extract_pages_with_password(path: &Path, password: &[u8]) -> Result<Vec<Vec<PdfItem>>> {
    let mut doc = lopdf::Document::load(path)?;
    if doc.is_encrypted() {
        doc.decrypt(password)?;
    }

    let pages = doc.get_pages();
    if pages.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_raw: Vec<RawPdfItem> = Vec::new();

    for page_num in pages.keys() {
        let page_text = doc.extract_text(&[*page_num]).unwrap_or_default();
        let y_offset = (*page_num as f64 - 1.0) * 10_000.0;
        for (line_idx, line) in page_text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let y = y_offset + (line_idx as f64 * 15.0);
            all_raw.push(RawPdfItem::new(line, 0.0, y, (line.len() as f64) * 6.0));
        }
    }

    log::debug!(
        "[TextExtractor] (pwd) {} pages → {} raw lines",
        pages.len(),
        all_raw.len()
    );

    Ok(cluster_into_rows(all_raw, 5.0))
}

pub fn extract_full_text_with_password(path: &Path, password: &[u8]) -> String {
    let mut doc = match lopdf::Document::load(path) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[TextExtractor] load failed: {}", e);
            return String::new();
        }
    };
    if doc.is_encrypted() {
        if let Err(e) = doc.decrypt(password) {
            log::warn!("[TextExtractor] decrypt failed: {}", e);
            return String::new();
        }
    }

    let pages = doc.get_pages();
    let mut parts = Vec::with_capacity(pages.len());

    for page_num in pages.keys() {
        match doc.extract_text(&[*page_num]) {
            Ok(t) => parts.push(t),
            Err(e) => log::debug!("[TextExtractor] (pwd) page {} error: {}", page_num, e),
        }
    }

    parts.join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::row_builder::RawPdfItem;

    // Integration tests (real PDFs) are run manually.
    // Unit tests cover the clustering logic used by extract_pages.

    #[test]
    fn single_line_becomes_one_row_one_item() {
        let items = vec![RawPdfItem::new(
            "03/04/2024 SALARY 50000.00 95000.00",
            0.0,
            0.0,
            200.0,
        )];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        assert!(rows[0][0].text.contains("SALARY"));
    }

    #[test]
    fn multiple_lines_each_become_own_row() {
        let items: Vec<RawPdfItem> = (0..5)
            .map(|i| RawPdfItem::new(format!("line {}", i), 0.0, (i as f64) * 15.0, 60.0))
            .collect();
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 5, "each line (Y spaced by 15) → separate row");
    }

    #[test]
    fn empty_text_produces_empty_rows() {
        let rows = cluster_into_rows(vec![], 5.0);
        assert!(rows.is_empty());
    }

    #[test]
    fn all_items_at_x_zero_means_fw_format() {
        // All items at X=0 → is_fw_format should return true
        use crate::parser::pdf_parser::is_fw_format;
        let rows: Vec<Vec<crate::parser::column_detector::PdfItem>> = (0..10)
            .map(|i| {
                vec![crate::parser::column_detector::PdfItem {
                    x: 0.0,
                    text: format!("{:02}/01/2024 PAYMENT 5000.00 95000.00", i + 1),
                    w: 300.0,
                }]
            })
            .collect();
        assert!(
            is_fw_format(&rows),
            "all-X=0 rows should be detected as FW format"
        );
    }
}
