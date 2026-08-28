// ocr_extractor.rs — Scanned PDF / image text extraction via Tesseract CLI.
// Requires: tesseract is installed and on PATH.
// Falls back gracefully when tesseract is not found.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::parser::column_detector::PdfItem;
use crate::parser::row_builder::{cluster_into_rows, RawPdfItem};

/// Image file extensions that Tesseract can read natively.
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "tiff", "tif", "bmp"];

/// Extract text from an image file (PNG/JPG/TIFF/BMP) via Tesseract CLI.
/// Tesseract reads image files natively — no pre-conversion needed.
pub fn extract_image_via_tesseract(img_path: &Path) -> Option<String> {
    let output = Command::new("tesseract")
        .arg(img_path)
        .arg("stdout")
        .arg("--dpi")
        .arg("300")
        .arg("-l")
        .arg("eng")
        .output()
        .ok()?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !text.trim().is_empty() {
            log::info!(
                "[OCR] image: extracted {} chars from {:?}",
                text.len(),
                img_path
            );
            return Some(text);
        }
    }

    log::warn!(
        "[OCR] image: tesseract returned no text for {:?}: {}",
        img_path,
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

/// Try to extract text from a PDF using the Tesseract OCR CLI.
/// Tesseract does not natively read PDF — we rely on it accepting stdin
/// or we use a two-step approach: if `pdftoppm` is available, rasterize first.
/// Simpler fallback: call `tesseract <path> stdout pdf` which works when
/// tesseract is built with the PDF input plugin (libtesseract with LEPTONICA).
///
/// Returns None if tesseract is not available or produces empty output.
pub fn extract_via_tesseract(pdf_path: &Path) -> Option<String> {
    // Primary: tesseract with PDF input support (tesseract ≥ 4.x with pdf plugin)
    let output = Command::new("tesseract")
        .arg(pdf_path)
        .arg("stdout")
        .arg("--dpi")
        .arg("300")
        .arg("-l")
        .arg("eng")
        .output()
        .ok()?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !text.trim().is_empty() {
            log::info!(
                "[OCR] tesseract extracted {} chars from {:?}",
                text.len(),
                pdf_path
            );
            return Some(text);
        }
    }

    log::warn!(
        "[OCR] tesseract returned no text for {:?}: {}",
        pdf_path,
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

// ── Positional (render + OCR) extraction ────────────────────────────────────
//
// Real bug fixed here (ICICI Bank Wealth Management, 2026-08-28): that
// statement's page content is 100% vector line-art — every character on
// every one of its 36 pages is drawn as filled bezier-curve/line-segment
// paths (`m`/`l`/`c`/`h`/`f`/`f*` operators), never via `Tj`/`TJ`/`'`/`"`.
// Confirmed directly against the decoded content stream: zero text-showing
// operators anywhere, and `doc.get_page_fonts()` finds zero fonts on every
// page (the page's only `/Resources` entry is a small logo `/XObject`).
// There is no embedded text to extract by *any* content-stream-walking
// method — `text_extractor::extract_page_text`'s superset-of-lopdf approach
// (see that module's doc comment) still returns nothing, correctly, because
// there is genuinely nothing there. The only way to recover this statement's
// data is to render the page as it visually displays and OCR that.
//
// `extract_via_tesseract` above (Tesseract's own built-in PDF reading) can't
// do this either: Tesseract/Leptonica's native PDF support only reads
// simple raster-image PDFs, and needs an external Ghostscript on PATH to
// rasterize anything else (confirmed: `tesseract file.pdf stdout pdf` fails
// with "Pdf reading is not supported" on a stock Tesseract install with no
// Ghostscript). `extract_pages_via_ocr` instead rasterizes each page itself
// via the `mutool` (MuPDF) CLI — a real vector-graphics-capable PDF
// renderer, so it renders this file's paths correctly — then runs
// Tesseract's TSV word-box output (not plain `stdout` text) on each
// resulting image, so every word keeps its on-page X/Y position. Those
// positions feed into the exact same `RawPdfItem`/`cluster_into_rows` row
// builder `text_extractor::extract_pages` uses for embedded text, producing
// the same `Vec<Vec<PdfItem>>` shape — so the result plugs straight into
// `pdf_parser::parse_pdf_rows`'s existing column-boundary detection
// (Date/Particulars/Deposits/Withdrawals/Balance) instead of needing a
// bespoke one-off parser, and instead of falling back to flat-text
// heuristics (`ocr_parser::parse_ocr_text`) that can't tell a Deposits
// column from a Withdrawals column without real column positions.
//
// Requires `mutool` (MuPDF, e.g. `winget install ArtifexSoftware.mutool`)
// on PATH in addition to Tesseract — same "external dependency, graceful
// degrade if absent" contract as the rest of this module.

/// Render resolution for OCR — high enough for Tesseract to read narrow
/// bank-statement fonts reliably without pages taking excessively long to
/// render/OCR. Matches the `--dpi` value already used for image/PDF OCR
/// elsewhere in this module.
const RENDER_DPI: u32 = 300;

/// Rasterize every page of `pdf_path` to `<tmpdir>/page<N>.png` via the
/// `mutool` CLI. Returns the temp directory (caller's responsibility to
/// remove) and the PNG paths in page order. `None` if `mutool` isn't on
/// PATH or the render fails outright — graceful degrade, same contract as
/// `extract_via_tesseract`.
fn rasterize_pdf_pages(pdf_path: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "bsp_ocr_render_{}_{}",
        std::process::id(),
        nanos
    ));
    if std::fs::create_dir_all(&dir).is_err() {
        log::warn!("[OCR] could not create render temp dir {:?}", dir);
        return None;
    }

    let pattern = dir.join("page%d.png");
    let run = Command::new("mutool")
        .arg("draw")
        .arg("-o")
        .arg(&pattern)
        .arg("-r")
        .arg(RENDER_DPI.to_string())
        .arg(pdf_path)
        .output();
    match run {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            log::warn!(
                "[OCR] mutool draw failed for {:?}: {}",
                pdf_path,
                String::from_utf8_lossy(&out.stderr)
            );
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        Err(e) => {
            log::warn!(
                "[OCR] mutool not available ({e}) — cannot rasterize vector-only PDF {:?}",
                pdf_path
            );
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
    }

    let mut pages: Vec<(u32, PathBuf)> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                let stem = path.file_stem()?.to_str()?.to_string();
                let num: u32 = stem.strip_prefix("page")?.parse().ok()?;
                Some((num, path))
            })
            .collect(),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
    };
    if pages.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    pages.sort_by_key(|(n, _)| *n);
    Some((dir, pages.into_iter().map(|(_, p)| p).collect()))
}

/// Run `tesseract <img> stdout tsv` and parse its word-level (level 5) rows
/// into `RawPdfItem`s, with pixel coordinates (at `RENDER_DPI`) rescaled to
/// PDF points (72/inch) so they're directly comparable to — and usable with
/// the same `cluster_into_rows(_, 5.0)` tolerance as — embedded-text item
/// coordinates from `text_extractor`.
fn ocr_words_tsv(img_path: &Path) -> Vec<RawPdfItem> {
    let output = match Command::new("tesseract")
        .arg(img_path)
        .arg("stdout")
        .arg("--dpi")
        .arg(RENDER_DPI.to_string())
        .arg("-l")
        .arg("eng")
        .arg("tsv")
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            log::warn!(
                "[OCR] tesseract tsv failed for {:?}: {}",
                img_path,
                String::from_utf8_lossy(&o.stderr)
            );
            return Vec::new();
        }
        Err(e) => {
            log::warn!("[OCR] tesseract not available ({e})");
            return Vec::new();
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let scale = 72.0 / RENDER_DPI as f64;
    let mut items = Vec::new();
    for line in text.lines().skip(1) {
        // TSV header: level page_num block_num par_num line_num word_num
        //             left top width height conf text
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 || cols[0] != "5" {
            continue; // only word-level (5) rows carry real text
        }
        let word = cols[11].trim();
        if word.is_empty() {
            continue;
        }
        let left: f64 = cols[6].parse().unwrap_or(0.0);
        let top: f64 = cols[7].parse().unwrap_or(0.0);
        let width: f64 = cols[8].parse().unwrap_or(0.0);
        items.push(RawPdfItem::new(word, left * scale, top * scale, width * scale));
    }
    items
}

/// Full vector-only-PDF fallback (see module-section doc comment above):
/// rasterize every page via `mutool`, OCR each page's words with position
/// via Tesseract, and cluster into the same `Vec<Vec<PdfItem>>` row shape
/// `text_extractor::extract_pages` produces — so the result feeds straight
/// into `pdf_parser::parse_pdf_rows` unchanged. Empty on any failure
/// (`mutool`/`tesseract` unavailable, zero pages, …) — same graceful-degrade
/// contract as the rest of this module; the caller falls through to the
/// existing flat-text OCR tier.
pub fn extract_pages_via_ocr(pdf_path: &Path) -> Vec<Vec<PdfItem>> {
    let Some((dir, pages)) = rasterize_pdf_pages(pdf_path) else {
        return Vec::new();
    };

    let mut all_raw: Vec<RawPdfItem> = Vec::new();
    for (idx, page_png) in pages.iter().enumerate() {
        let page_num = idx + 1;
        // Same page-separation convention as text_extractor::extract_pages
        // (a large fixed offset per page so rows from different pages never
        // cluster together).
        let y_offset = (page_num as f64 - 1.0) * 10_000.0;
        let words = ocr_words_tsv(page_png);
        log::debug!(
            "[OCR] positional: page {}/{} -> {} words",
            page_num,
            pages.len(),
            words.len()
        );
        for w in words {
            all_raw.push(RawPdfItem::new(w.text, w.x, w.y + y_offset, w.w));
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    cluster_into_rows(all_raw, 5.0)
}
