# Phase 2 Completion Report — Integration Test Suite

**Date:** 2026-07-08
**Scope:** Complete end-to-end integration test suite against real bank-statement fixtures, covering the full workflow requested: PDF/Excel/OCR/CSV import, deduplication, classification, GST detection, narration cleaning, vendor detection, rules engine, database persistence, analytics, and export (Excel/CSV/Tally XML), plus regression tests for every Phase 1 bug fix.

This report also covers the "batch import Pause/Abort" feature implemented immediately before this integration-test task, in the same session.

---

## 1. Fixture inventory

Copied byte-for-byte (verified size-matched, no wildcards used) from the old Electron app's own `assets/` folder into `tests/fixtures/`:

| Format | Count | Location |
|---|---|---|
| PDF (real bank statements, 11 banks) | 11 | `tests/fixtures/bank_statements/*.pdf` |
| Excel (.xls) | 1 (`HDFC.xls`) | `tests/fixtures/bank_statements/HDFC.xls` |
| CSV (bank statement) | **0 — does not exist anywhere in either app's repo** | — |
| OCR image (png/jpg/tiff/bmp) | **0 — does not exist anywhere in either app's repo** | — |
| Ledger name/group CSV | 0 real fixture available | Created a small representative fixture: `tests/fixtures/ledgers/sample_ledgers.csv` (no real one exists in either repo; explicitly not a bank-statement substitute) |

CSV bank-statement import and OCR-image fixtures are marked `#[ignore]` with explicit reasons rather than silently omitted or faked. Tesseract is not installed on this machine (absent from `PATH`), so the image→text OCR step could not be exercised even if a fixture existed; the OCR text-*parsing* logic (`parse_ocr_text`) is still tested directly with realistic OCR-shaped text.

---

## 2. Files changed

**New test files (`tests/`):**
- `import_pipeline.rs` — PDF (2-stage pipeline), Excel, OCR-text, CSV-status (11 tests: 3 pass, 4 ignored/documented-bugs, 1 ignored/not-implemented, 1 helper)
- `processing_pipeline.rs` — vendor normalization, narration cleaning, classification, duplicate detection, GST analysis, full chain (6 tests, all pass)
- `analytics_export_persistence.rs` — analytics, DB persistence, re-import idempotency, multi-client isolation, Excel/CSV/Tally-XML/generic-XML export (8 tests: 7 pass, 1 ignored/documented-bug)
- `ledger_reconciliation_errors.rs` — ledger CSV import, reconciliation engine, error handling on malformed input (11 tests, all pass)
- `phase1_regressions.rs` — Settings-drives-behavior, classification_rules dedup constraint (4 tests, all pass)

**New fixtures:**
- `tests/fixtures/bank_statements/` — 11 PDFs + 1 XLS, real files from the old app
- `tests/fixtures/ledgers/sample_ledgers.csv` — small representative fixture (no real one exists)

**New diagnostic examples:**
- `examples/pdf_debug_probe.rs`, `examples/pdf_batch_probe.rs` — reproduce the PDF extraction bugs found below; referenced from the tests' doc comments as the way to independently re-verify them

**Modified (1 line, pre-existing bug found via `cargo clippy --all-targets --all-features`):**
- `src/parser/ocr_parser.rs:619` — a pre-existing test asserted `real.len() >= 0`, which is always true for a `usize` and is `#[deny]`'d by clippy's `absurd_extreme_comparisons` lint, hard-failing `cargo clippy`. Fixed to assert what the test's own comment said it intended to check (`!real.is_empty()`). This was the *only* code change required to make `cargo clippy --all-targets --all-features` exit 0; every other clippy warning is pre-existing and outside this phase's scope, left untouched per instruction.

**Also included from immediately before this task (same session, same quality gate):**
- `src/ui/mod.rs`, `src/main.rs`, `ui/app.slint`, `ui/main_screen.slint` — real batch-import Pause/Abort (verified genuine parity gap vs. the old app; see prior commit for full detail)

---

## 3. Real bugs found by this suite (not fixed — out of scope for an integration-test-only change)

Testing against real fixtures surfaced **four previously-undiscovered production bugs**, none of which were flagged in `PROJECT_AUDIT_2026-07-06.md`. Each is documented in an `#[ignore]`d test with a clear reason, a doc comment explaining the root cause, and reproduction steps — not silently skipped.

### 🔴 Highest severity: cross-client transaction ID collision (data corruption risk)

`transactions.id` is the table's **sole** primary key (not composite with `client_id`), and ids are generated purely from in-file row position (`format!("t_{}_{}", i, txns.len())`, `excel_parser.rs:735`) with no client-specific or file-specific salt. **Two different clients whose imports happen to produce matching ids will silently overwrite and reassign each other's transactions** via `upsert_transactions`'s `INSERT OR REPLACE`. This is guaranteed for the same file imported for two clients, and plausible for two different files with the same row count. In a multi-client accounting tool, this is a real data-integrity and cross-tenant-leakage risk, not just a missing feature.
→ `analytics_export_persistence.rs::multi_client_transaction_data_is_fully_isolated`

### 🟠 PDF text extraction: Identity-H/CID font not decoded (4 of 11 real fixtures)

`BOB.pdf`, `ICICI Bank.pdf`, `IDFCFIRSTBankstatement.pdf`, `Union Bank.pdf` embed their transaction-table text using an Identity-H/CID-keyed font `lopdf`'s extractor cannot decode — it returns the literal placeholder string `"?Identity-H Unimplemented?"` instead of real characters. Worse than a missing-Tesseract gap: `run_pdf_ocr_pipeline`'s Tesseract-fallback check is a bare `full_text.trim().is_empty()`, and this garbage text is *not* empty, so real OCR is never attempted even on a machine with Tesseract installed. A real user with any of these 4 exact PDFs cannot load them today, with no actionable error. (A pre-existing, never-fixed diagnostic check for this exact string already existed at `src/bin/pdf_diag.rs:25`, confirming this was known before — just never surfaced in the audit.)
→ `import_pipeline.rs::pdf_fixtures_with_identity_h_encoding_produce_zero_transactions`

### 🟡 PDF text extraction: near-empty text not detected as needing OCR (1 fixture)

`Cosmos Co-operative.pdf`'s embedded text layer is 144 characters of page furniture ("Date Stamp Manager") with the real transaction table entirely absent. Same root cause as above (naive `is_empty()` check), different symptom.
→ `import_pipeline.rs::cosmos_pdf_exposes_a_missing_ocr_fallback_for_near_empty_text`

### 🟡 PDF text extraction: zero pages extracted (1 fixture, largest file)

`ICICI Bank Wealth management.pdf` (6.2MB) fails at the very first extraction step — `extract_pages` returns zero rows, no error. Time-boxed per instruction rather than root-caused further; most likely a size/structural-complexity limit in `lopdf`'s parsing.
→ `import_pipeline.rs::icici_wealth_management_pdf_extracts_zero_pages`

### Net PDF result: **5 of 11 real fixtures parse correctly** through the real two-stage pipeline (`Bank of Maharashtra.pdf`, `IDBI Bank.pdf`, `Kotak Bank.pdf`, `Mahanager Co-operative bank.pdf`, `SBI.pdf`). This is a materially worse real-world success rate than the audit's "PDF Import 95%" estimate suggested — that estimate was based on code review, not real-fixture testing.

---

## 4. Total statistics

| Metric | Count |
|---|---|
| **Total test functions** | **516** |
| Unit tests (`cargo test --lib`) | 471 |
| Unit tests (bin crate, `main.rs` — first-ever coverage, added with the Pause/Abort feature) | 8 |
| Integration tests (5 new files under `tests/`) | 36 (31 passing + 5 `#[ignore]`d/documented) |
| Doctests | 1 |
| **Passing** | **511** |
| **Ignored (all documented with a reason, not silent)** | **5** — 4 real bugs above + 1 not-implemented-in-either-app (CSV bank-statement import) |
| **Failing** | **0** |

**Build status:** `cargo build` — clean, 0 errors, only pre-existing unrelated warnings.
**Clippy status:** `cargo clippy --all-targets --all-features` — clean, 0 errors (1 pre-existing `deny`-level hard error fixed; all other pre-existing warnings across `main.rs`/`settings.rs`/`migration/transformer.rs`/etc. are untouched, out of this phase's scope per instruction).
**Format status:** `cargo fmt --check` — clean on every new/touched file.

---

## 5. Remaining production tasks

**From this phase's findings (new, not previously known):**
1. Fix the cross-client transaction-id collision (needs a real schema change — likely a composite key or a client-scoped id generator).
2. Fix `run_pdf_ocr_pipeline`'s OCR-fallback trigger to detect "text extraction produced garbage/too-little text," not just "text is empty" (would recover the Identity-H and near-empty-text cases — likely not the zero-pages case).
3. Investigate `lopdf`'s handling of large/complex PDFs (`ICICI Bank Wealth management.pdf`).

**Carried over from `PROJECT_AUDIT_2026-07-06.md` (previously known, still open):**
4. README, user manual, basic CI.
5. Packaging: installer, auto-update, version bump past `0.1.0`.
6. Optional: surface already-computed-but-discarded GST/narration fields (low priority — old app has the same dead-code pattern).

---

## 6. Commit

All Phase 2 changes (batch Pause/Abort feature + full integration test suite + the one clippy fix) will be committed together with a detailed message immediately following this report.

**Not starting Phase 3. Waiting for approval.**
