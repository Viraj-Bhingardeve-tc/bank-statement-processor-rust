// ocr_extractor.rs — Scanned PDF / image text extraction via Tesseract CLI.
// Requires: tesseract is installed and on PATH.
// Falls back gracefully when tesseract is not found.

use std::path::Path;
use std::process::Command;

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
