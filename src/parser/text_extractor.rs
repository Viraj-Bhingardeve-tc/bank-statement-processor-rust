//! text_extractor.rs — PDF → `Vec<Vec<PdfItem>>` using lopdf.
//!
//! **Limitation**: lopdf's `extract_text()` returns flat text per page with no
//! X/Y positional information.  This extractor approximates positions by treating
//! each line as a row and each word as an item with X = character_offset × 6.
//!
//! For true column detection (HDFC, SBI, ICICI, etc.) pdfium-render is required
//! (planned for Phase 4).  This extractor is sufficient for:
//!   • Fixed-width format PDFs (Cosmos, some co-op banks) — all items share X≈0.
//!   • Feeding text into bank detection (text content only).
//!   • Line-by-line OCR text output.
//!
//! All downstream parsing logic is tested independently with synthetic PdfItem data.

use std::path::Path;

use anyhow::Result;

use crate::parser::{
    column_detector::PdfItem,
    row_builder::{cluster_into_rows, RawPdfItem},
};

// ── extract_pages ─────────────────────────────────────────────────────────────

/// Extract text items from a PDF file and cluster them into rows.
///
/// Uses lopdf to read raw text per page.  Each line of text becomes a row;
/// each word within the line becomes an item with approximate X position.
/// Y coordinates are assigned as `line_number × 15` (per-page, reset each page
/// then offset by `page_number × 1000` so items from different pages don't merge).
///
/// **Known limitations** (requires pdfium-render to fix):
/// - X positions are approximated from character offsets, not real PDF points.
/// - Multi-column layouts (standard bank PDFs) will not cluster correctly.
/// - Fixed-width PDFs (all text at x≈0) work correctly.
pub fn extract_pages(path: &Path) -> Result<Vec<Vec<PdfItem>>> {
    let doc = lopdf::Document::load(path)?;

    let mut all_raw: Vec<RawPdfItem> = Vec::new();
    let page_ids: Vec<lopdf::ObjectId> = doc.page_iter().collect();

    for (page_idx, &page_id) in page_ids.iter().enumerate() {
        let page_text = doc
            .extract_text(&[page_id.0 as u32])
            .unwrap_or_default();

        let y_offset = page_idx as f64 * 1000.0;

        for (line_idx, line) in page_text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() { continue; }

            let y = y_offset + (line_idx as f64 * 15.0);

            // Build one item per word with approximate X from character position.
            // For FW-format PDFs (single text column), all words share X≈0.
            let mut char_pos = 0usize;
            for word in line.split_whitespace() {
                if word.is_empty() { continue; }
                let x = char_pos as f64 * 6.0; // ~6pt per character (approximate)
                all_raw.push(RawPdfItem::new(word, x, y, (word.len() as f64) * 6.0));
                char_pos += word.len() + 1;
            }
        }
    }

    Ok(cluster_into_rows(all_raw, 5.0))
}

/// Extract all page text as one joined string (for bank detection).
pub fn extract_full_text(path: &Path) -> String {
    match lopdf::Document::load(path) {
        Err(_) => String::new(),
        Ok(doc) => {
            let page_ids: Vec<lopdf::ObjectId> = doc.page_iter().collect();
            page_ids.iter().filter_map(|&id| {
                doc.extract_text(&[id.0 as u32]).ok()
            }).collect::<Vec<_>>().join("\n")
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::row_builder::RawPdfItem;
    use crate::parser::column_detector::PdfItem;

    // Tests for extract_pages are integration-level (require actual PDF files).
    // All unit tests are in the modules that consume Vec<Vec<PdfItem>>.

    // ── cluster_into_rows round-trip (sanity) ─────────────────────────────────

    #[test]
    fn words_on_same_line_cluster_into_one_row() {
        // Simulate what extract_pages produces for "Date Narration Balance"
        let items = vec![
            RawPdfItem::new("Date",      0.0,  10.0, 25.0),
            RawPdfItem::new("Narration", 30.0, 10.0, 60.0),
            RawPdfItem::new("Balance",   95.0, 10.0, 45.0),
        ];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 1, "all on Y=10 → one row");
        assert_eq!(rows[0].len(), 3);
    }

    #[test]
    fn words_on_different_lines_make_separate_rows() {
        let items = vec![
            RawPdfItem::new("Header",  0.0, 10.0, 40.0),
            RawPdfItem::new("DataRow", 0.0, 25.0, 50.0),
        ];
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 2, "Y diff = 15 > 5 → separate rows");
    }

    #[test]
    fn fw_pdf_all_items_near_x_zero() {
        // Fixed-width PDF: simulate lopdf output where all text is at x≈0
        let items: Vec<RawPdfItem> = (0..5).map(|i| {
            RawPdfItem::new(
                &format!("{:02}/01/2024 NARRATION 5000.00 95000.00Cr", i+1),
                0.0, (i as f64) * 15.0, 300.0
            )
        }).collect();
        let rows = cluster_into_rows(items, 5.0);
        assert_eq!(rows.len(), 5, "5 lines → 5 rows");
        // Each row has one item (the full line)
        assert_eq!(rows[0].len(), 1);
    }

    // ── PdfItem carries correct values ────────────────────────────────────────

    #[test]
    fn raw_item_converts_to_pdf_item_correctly() {
        let raw = RawPdfItem::new("Test", 42.0, 100.0, 30.0);
        let rows = cluster_into_rows(vec![raw], 5.0);
        let item = &rows[0][0];
        assert!((item.x - 42.0).abs() < 0.001);
        assert_eq!(item.text, "Test");
        assert!((item.w - 30.0).abs() < 0.001);
        // Y is not present on PdfItem — only used for clustering
    }
}
