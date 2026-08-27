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
//!
//! **Bug fixed (Cosmos Co-operative Bank, 2026-08-27)**: `lopdf::Document::extract_text`
//! (see `lopdf::parser_aux::extract_text_chunks_from_page`) only recognizes the `Tj`/`TJ`
//! text-showing operators — it silently ignores `'` (quote: move to next line and show
//! text) and `"` (double-quote: set spacing, move to next line, and show text). Both are
//! ordinary, spec-legal PDF content-stream operators (PDF 1.7 §9.4.3), and the Cosmos
//! statement's PDF generator uses `'` for essentially every line of the transaction
//! table's text — verified directly against this file's decoded content stream via
//! `doc.get_page_content()`: the real text (`"THE COSMOS CO-OPERATIVE BANK LTD"`, every
//! transaction row, …) is right there as `(...)'` operations, but `extract_text()` only
//! ever emits the text from the page's lone `Tj` call — one placeholder glyph — because
//! that's the only operator it looks for. Silent, not an error: `extract_full_text`
//! returns a short-but-non-empty string (a few real `Tj` fragments elsewhere on the page,
//! e.g. a footer note), which is enough to skip `main.rs`'s "is the text empty? fall back
//! to Tesseract OCR" check, so the app used to hand this near-empty text straight to
//! `ocr_parser::parse_ocr_text` and get zero transactions — this is not a missing-OCR
//! problem, it's a text-extraction one; real embedded text is present and extractable.
//!
//! `extract_page_text` below re-implements lopdf's own per-page walk (same Tf-driven
//! encoding tracking, same `Document::decode_text`/`get_page_fonts`/`get_page_content`
//! calls lopdf's internal version uses) but also handles `'`, `"`, and `T*`, so it is a
//! strict superset of what `doc.extract_text()` produces: any PDF that only used
//! `Tj`/`TJ` (every other bank fixture this codebase has been tested against) gets
//! byte-for-byte the same output, and a PDF that also uses `'`/`"` (Cosmos) now gets its
//! text too, instead of it being silently dropped.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use lopdf::{content::Content, Document, Encoding, Object, ObjectId};

use crate::parser::{
    column_detector::PdfItem,
    row_builder::{cluster_into_rows, RawPdfItem},
};

/// Append the text carried by `Tj`/`TJ`/`'`/`"` operands to `text`, decoded via `encoding`.
/// Mirrors lopdf's own private `collect_text` helper (`parser_aux.rs`) exactly for
/// `Object::String`/`Object::Array`/`Object::Integer` handling, so `Tj`/`TJ` output here
/// is identical to `doc.extract_text()`'s.
fn collect_text(text: &mut String, encoding: &Encoding, operands: &[Object]) {
    for operand in operands {
        match operand {
            Object::String(bytes, _) => {
                if let Ok(s) = Document::decode_text(encoding, bytes) {
                    text.push_str(&s);
                }
            }
            Object::Array(arr) => collect_text(text, encoding, arr),
            Object::Integer(i) if *i < -100 => text.push(' '),
            _ => {}
        }
    }
}

/// Extract one page's text, walking its decoded content-stream operations directly
/// instead of relying on `lopdf::Document::extract_text` (see module doc comment for
/// why: that function silently drops `'`/`"`-drawn text). Returns `""` on any error
/// (missing content stream, undecodable fonts, …) — same "best effort, never panic"
/// contract `doc.extract_text(...).unwrap_or_default()` had at every existing call site.
fn extract_page_text(doc: &Document, page_id: ObjectId) -> String {
    let fonts = match doc.get_page_fonts(page_id) {
        Ok(f) => f,
        Err(e) => {
            log::debug!("[TextExtractor] get_page_fonts failed: {}", e);
            BTreeMap::new()
        }
    };
    let encodings: BTreeMap<Vec<u8>, Encoding> = fonts
        .into_iter()
        .filter_map(|(name, font)| font.get_font_encoding(doc).ok().map(|enc| (name, enc)))
        .collect();

    let content_data = match doc.get_page_content(page_id) {
        Ok(d) => d,
        Err(e) => {
            log::debug!("[TextExtractor] get_page_content failed: {}", e);
            return String::new();
        }
    };
    let content = match Content::decode(&content_data) {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[TextExtractor] Content::decode failed: {}", e);
            return String::new();
        }
    };

    let mut out = String::new();
    let mut current_encoding: Option<&Encoding> = None;
    for operation in &content.operations {
        match operation.operator.as_str() {
            "Tf" => {
                if let Some(Ok(font_name)) = operation.operands.first().map(Object::as_name) {
                    current_encoding = encodings.get(font_name);
                }
            }
            "Tj" | "TJ" => {
                if let Some(encoding) = current_encoding {
                    collect_text(&mut out, encoding, &operation.operands);
                }
            }
            // `'` — "move to the next line and show a text string" (PDF 1.7 §9.4.3,
            // Table 209): equivalent to `T*` followed by `Tj`. lopdf's own
            // `extract_text` does not implement this operator at all.
            "'" => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if let Some(encoding) = current_encoding {
                    collect_text(&mut out, encoding, &operation.operands);
                }
            }
            // `"` — "set word/char spacing, move to next line, show text": operands
            // are `[aw ac string]`; only the trailing string operand is text.
            "\"" => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if let (Some(encoding), Some(s)) = (current_encoding, operation.operands.get(2)) {
                    collect_text(&mut out, encoding, std::slice::from_ref(s));
                }
            }
            "T*" => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            "ET" => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
    out
}

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

    for (page_num, page_id) in pages.iter() {
        let page_text = extract_page_text(&doc, *page_id);

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

    for page_id in pages.values() {
        parts.push(extract_page_text(&doc, *page_id));
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

    for (page_num, page_id) in pages.iter() {
        let page_text = extract_page_text(&doc, *page_id);
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

    for page_id in pages.values() {
        parts.push(extract_page_text(&doc, *page_id));
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
