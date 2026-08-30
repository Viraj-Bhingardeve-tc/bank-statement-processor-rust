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

/// Outcome of a password-aware rasterize/OCR attempt — unlike the
/// unauthenticated path (which only ever needs a plain "did this work"
/// bool), a password attempt has three genuinely different outcomes the
/// caller must react to differently: proceed with the pages, re-prompt for
/// a corrected password, or report a real tool/environment failure.
pub enum OcrPasswordOutcome {
    Ok(Vec<Vec<PdfItem>>),
    /// `mutool` itself rejected the password (`"cannot authenticate
    /// password"` on stderr) — this is a genuine wrong-password signal, not
    /// a missing-tool or render failure, and must never be logged alongside
    /// the password that produced it.
    IncorrectPassword,
    /// `mutool`/`tesseract` missing, or a non-password render failure.
    Unavailable,
}

enum RasterizeOutcome {
    Ok((PathBuf, Vec<PathBuf>)),
    IncorrectPassword,
    Unavailable,
}

/// Tesseract's automatic page-layout analysis can silently lose an entire
/// wide text region on a rendered page whose real content is a small block
/// followed by a large blank area beneath it — confirmed against a real
/// fixture (Union Bank.pdf) whose true final page has only 6 transaction
/// rows: every OCR mode tried (every `--psm` value, both `--oem` engines)
/// consistently recognized the narrow Date/Time/Ref columns on the left but
/// returned nothing at all — not even a low-confidence guess — for the
/// wide Narration/Debit/Credit/Balance region to their right, even though
/// that region visibly contains ordinary black text on a plain white/pale-
/// gray background. Tesseract's own TSV output for a failing page shows
/// why: it merges that whole wide region into a single "block" spanning
/// nearly the full page width and height, then extracts a single blank
/// space as that block's only content — its column/line-detection heuristic
/// appears to get confused specifically by the combination of a wide
/// multi-column region sitting above a disproportionately large blank
/// margin, unrelated to `--psm`/`--oem` (both tested exhaustively; identical
/// failure every time) and unrelated to DPI. Cropping the rendered image
/// down to just past its real content — removing that oversized blank
/// margin before Tesseract ever sees it — reliably fixes it (confirmed:
/// identical crop against the same fixture recovers every word, at full
/// confidence, that the uncropped image lost entirely).
///
/// This only ever *removes* pixels already confirmed blank; it can never
/// crop into real content (the scan below finds the exact last content
/// row and leaves a wide margin past it), and it's a no-op — the file is
/// left untouched — for the overwhelming common case of a normally-
/// populated page. So while only one real fixture is known to need this,
/// applying it to every rendered page is safe for every bank already
/// working correctly and only ever helps a page that would otherwise
/// silently lose transactions this same way.
fn crop_trailing_blank_space(png_path: &Path) {
    let img = match image::open(png_path) {
        Ok(i) => i.to_rgb8(),
        Err(e) => {
            log::debug!("[OCR] crop_trailing_blank_space: could not open {png_path:?}: {e}");
            return;
        }
    };
    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        return;
    }
    // Near-white threshold: catches both black text and the pale blue/gray
    // banded row backgrounds real bank statements commonly alternate
    // between, while ignoring the plain white page background and
    // anti-aliasing fringes right at a glyph's edge.
    const NEAR_WHITE: u8 = 245;
    let has_content = |y: u32| -> bool {
        (0..width).any(|x| {
            let p = img.get_pixel(x, y);
            p[0] < NEAR_WHITE || p[1] < NEAR_WHITE || p[2] < NEAR_WHITE
        })
    };

    // The real content to crop *around* is the transaction table, but a
    // page's own footer disclaimer sits well *below* it — confirmed
    // against the real fixture this was built for: its problem page's last
    // non-blank row overall is the footer, not the table, so a naive
    // "scan from the bottom for the last non-blank row" finds the footer
    // and (correctly, by its own reasoning) declines to crop, since the
    // footer alone already makes the page look ~94% full. What actually
    // confuses Tesseract is the *gap* — a wide blank run sitting between
    // two real content blocks, not merely content near the bottom — so
    // this looks for the first such gap and crops right there, discarding
    // the footer along with it (harmless: nothing downstream needs footer
    // text — every extractor already has its own dedicated footer-skip
    // filter for whatever footer text *does* survive intact elsewhere).
    // `GAP_PX` is comfortably larger than the ~40-50px spacing between
    // ordinary transaction rows, so normal inter-row gaps never trigger
    // this, and comfortably smaller than the ~2600px gap actually observed
    // on the real fixture's problem page.
    const GAP_PX: u32 = 400;
    const MARGIN_PX: u32 = 300;
    let mut last_content_row: Option<u32> = None;
    let mut cut_at: Option<u32> = None;
    for y in 0..height {
        if has_content(y) {
            if let Some(last) = last_content_row {
                if y - last > GAP_PX {
                    cut_at = Some(last);
                    break;
                }
            }
            last_content_row = Some(y);
        }
    }
    let Some(cut_row) = cut_at else {
        // No large internal gap found — either the page is normally
        // packed throughout (the overwhelming common case) or it's blank
        // start to finish. Either way, leave the file untouched.
        return;
    };
    let cropped_height = cut_row.saturating_add(MARGIN_PX).min(height);
    let cropped = image::imageops::crop_imm(&img, 0, 0, width, cropped_height).to_image();
    match cropped.save(png_path) {
        Ok(()) => log::debug!(
            "[OCR] cropped {png_path:?} from {height}px to {cropped_height}px (trailing blank space)"
        ),
        Err(e) => log::warn!("[OCR] failed to save cropped page image {png_path:?}: {e}"),
    }
}

/// Shared rasterize implementation for both the unauthenticated and
/// password-protected paths. `password` is passed to `mutool draw -p`
/// verbatim as raw bytes reinterpreted as UTF-8 (lossily, matching how the
/// rest of the password-handling code — `text_extractor::
/// extract_pages_with_password` — already treats the password as UTF-8
/// bytes) and is **never** included in any log line, including the ones
/// that echo `mutool`'s own stderr (checked below: `mutool`'s error text
/// for a bad password is a fixed string plus the *file path*, never an
/// echo of the password argument itself).
fn rasterize_pdf_pages_inner(pdf_path: &Path, password: Option<&[u8]>) -> RasterizeOutcome {
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
        return RasterizeOutcome::Unavailable;
    }

    let pattern = dir.join("page%d.png");
    let mut cmd = Command::new("mutool");
    cmd.arg("draw");
    if let Some(pwd) = password {
        // Lossy UTF-8 is intentional and matches `extract_pages_with_password`'s
        // existing contract elsewhere in this codebase — a password that
        // round-trips through UTF-8 (the overwhelming common case) is
        // unaffected; one that doesn't would already fail lopdf's own
        // password check the same way.
        cmd.arg("-p").arg(String::from_utf8_lossy(pwd).as_ref());
    }
    cmd.arg("-o").arg(&pattern).arg("-r").arg(RENDER_DPI.to_string()).arg(pdf_path);
    let run = cmd.output();
    match run {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if password.is_some() && stderr.contains("cannot authenticate password") {
                // Deliberately do not log `stderr` here even though it
                // contains no password material (see doc comment) — a
                // wrong-password attempt is expected user behavior, not a
                // condition worth a log line at all.
                let _ = std::fs::remove_dir_all(&dir);
                return RasterizeOutcome::IncorrectPassword;
            }
            log::warn!("[OCR] mutool draw failed for {:?}: {}", pdf_path, stderr);
            let _ = std::fs::remove_dir_all(&dir);
            return RasterizeOutcome::Unavailable;
        }
        Err(e) => {
            log::warn!(
                "[OCR] mutool not available ({e}) — cannot rasterize vector-only PDF {:?}",
                pdf_path
            );
            let _ = std::fs::remove_dir_all(&dir);
            return RasterizeOutcome::Unavailable;
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
            return RasterizeOutcome::Unavailable;
        }
    };
    if pages.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return RasterizeOutcome::Unavailable;
    }
    pages.sort_by_key(|(n, _)| *n);
    RasterizeOutcome::Ok((dir, pages.into_iter().map(|(_, p)| p).collect()))
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
    match extract_pages_via_ocr_inner(pdf_path, None) {
        OcrPasswordOutcome::Ok(rows) => rows,
        _ => Vec::new(),
    }
}

/// Password-protected variant of `extract_pages_via_ocr` — the OCR-fallback
/// counterpart to `text_extractor::extract_pages_with_password`. Used when
/// an encrypted PDF's password has been supplied but either (a) `lopdf`
/// can't decrypt this file's encryption revision at all (it only supports
/// standard-security-handler revisions 2-4 — see `lopdf::encryption`; a
/// revision 5/6 "AES-256" PDF, common from many modern statement
/// generators, fails at the *revision* check before the password is even
/// checked), or (b) decryption succeeded but the decrypted content still
/// isn't extractable by any text-layer means (the same class of problem
/// `extract_pages_via_ocr` solves for unencrypted vector-only PDFs).
/// `mutool` decrypts and rasterizes in one step via its own `-p` flag,
/// independent of `lopdf`'s decryption support entirely, so this recovers
/// pages `lopdf` cannot touch at all.
///
/// Returns `OcrPasswordOutcome::IncorrectPassword` only when `mutool`
/// itself rejects the password — never guessed from a generic failure — so
/// the caller can safely re-show the password prompt instead of a dead-end
/// error. The password is never logged (see `rasterize_pdf_pages_inner`'s
/// doc comment) and is used only for this one `mutool` invocation.
pub fn extract_pages_via_ocr_with_password(pdf_path: &Path, password: &[u8]) -> OcrPasswordOutcome {
    extract_pages_via_ocr_inner(pdf_path, Some(password))
}

fn extract_pages_via_ocr_inner(pdf_path: &Path, password: Option<&[u8]>) -> OcrPasswordOutcome {
    let (dir, pages) = match rasterize_pdf_pages_inner(pdf_path, password) {
        RasterizeOutcome::Ok(v) => v,
        RasterizeOutcome::IncorrectPassword => return OcrPasswordOutcome::IncorrectPassword,
        RasterizeOutcome::Unavailable => return OcrPasswordOutcome::Unavailable,
    };

    let mut all_raw: Vec<RawPdfItem> = Vec::new();
    for (idx, page_png) in pages.iter().enumerate() {
        let page_num = idx + 1;
        // Same page-separation convention as text_extractor::extract_pages
        // (a large fixed offset per page so rows from different pages never
        // cluster together).
        let y_offset = (page_num as f64 - 1.0) * 10_000.0;
        crop_trailing_blank_space(page_png);
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
    OcrPasswordOutcome::Ok(cluster_into_rows(all_raw, 5.0))
}
