# Production Readiness Audit — bank-statement-processor-rust

**Date:** 2026-06-22 · **Audited commit:** `e6e5e04` (main, clean tree) · **Method:** full direct repository inspection (file tree, `cargo build`, `cargo test`, git history, dependency manifest) plus six independent deep-dive passes over the parser, database/auth/security, export/classification/GST wiring, Slint UI, Rust engineering quality, and testing subsystems. Every finding below is backed by a file path and, where applicable, a line number. This is a point-in-time snapshot — re-verify against current code before acting on any specific citation if significant time has passed.

**Scope note:** this is a real, working, single-developer Rust + Slint desktop rewrite of an Electron app (`bank-statement-processing`, sibling directory), now 40 commits deep. It is **not**, in its current form, an enterprise system built for "millions of transactions" multi-tenant production use — it is a single-user, single-machine desktop tool with a genuinely capable parsing/classification core and significant gaps in security, data protection, and operability that must be closed before that framing is accurate. The rest of this report quantifies exactly where it stands.

---

## Executive Summary

**Overall Production Readiness: 42 / 100.** A capable, evidence-backed parsing and classification engine sits underneath an application that has no real access control, no data-at-rest encryption, no schema migration path, and several confirmed "computed-but-discarded" data bugs in its flagship GST/Tally features. None of the 6 audit passes found memory-safety issues (zero `unsafe`, panic risk is well-guarded) — the risk profile here is architectural and security-shaped, not "the app crashes."

**Top 6 findings, ranked:**

1. **CRITICAL — Auth is licensing, not security.** `src/auth/monthly_password.rs` derives its password from an HMAC secret key hardcoded in source (`SK_FRAGMENTS`, lines 18-27) — anyone with the binary can forge a valid password offline. There is no per-user identity, no lockout, nothing behind it but an open SQLite file.
2. **HIGH — No data-at-rest encryption.** Real bank account numbers, balances, and narrations sit in a plaintext SQLite file (`src/db/mod.rs:540`). No SQLCipher, no AES, nothing in `Cargo.toml` provides this.
3. **HIGH — GST and Tally-grouping data is computed and then silently discarded.** `gst_engine::analyse()` runs on every transaction; six of its seven fields (rate, type, amount, GSTINs, suggested ledger, confidence) are never read again (`src/classifier.rs:95-104`). The export wizard's GSTIN/financial-year/state-code/"include GST" fields are collected from the UI and never referenced inside the actual export generators (`src/export/tally.rs`, `src/export/accounting.rs`). A user filling in those fields sees zero effect on the exported file.
4. **HIGH — No schema migration framework.** No `PRAGMA user_version`, no migrations table. Schema evolution relies on best-effort `ALTER TABLE` calls whose errors are silently swallowed (`src/db/mod.rs:560-563`) — a destructive future schema change could fail silently and desync the app from its own database with no diagnostic.
5. **HIGH — DB write failures are silently swallowed in ~19 sites in `src/main.rs`** (`let _ = db::upsert_transactions(...)`, `let _ = db::delete_transaction(...)`, etc.), including the core "save a parsed statement" and "delete a transaction" paths. The UI can report success while nothing was actually persisted.
6. **CRITICAL gap, but already partly mitigated** — CSV is supported for ledger-master import but **not for bank-statement transaction import at all** (no `.csv` in the file-open filter, `src/main.rs:1107`), despite `csv` being a project dependency.

**One correction to the project's own history:** `PARITY_GAP_REPORT.md` (committed in repo root) claims the AI Consent modal is "entirely absent from Rust UI." This is false as of the audited commit — `main_screen.slint:2636-2710` implements a complete, well-written consent modal with a mandatory checkbox and an accurate (if slightly over-inclusive) disclosure of what data is sent to third-party AI providers. This is the second time this file has been caught stating something false about current code (see prior memory note on the 2026-06-19 audit) — **treat `PARITY_GAP_REPORT.md` as unreliable and retire it.**

---

## Phase 1 — Repository Discovery

### 1.1 Project structure

```
bank-statement-processor-rust/
├── Cargo.toml, Cargo.lock, build.rs        — bin+lib crate, Slint codegen
├── PARITY_GAP_REPORT.md                    — STALE, contradicted by current code (see above)
├── parity_audit_backup.patch (62KB)        — committed backup diff from a past session (repo hygiene debt)
├── .patch_a_ai_classifier.diff             — same category
├── assets/.gitkeep                         — no bundled assets/icons/fixtures
├── src/
│   ├── main.rs (3720 lines)                — orchestrator: every Slint callback handler
│   ├── lib.rs (9 lines)                    — exposes parser+engines for `--no-default-features` testing
│   ├── analytics.rs (410)                  — dashboard aggregates
│   ├── classifier.rs (539)                 — rule-based classification + dedup
│   ├── ai_classifier.rs (306)              — OpenAI/Claude/Gemini classification
│   ├── gst_engine.rs (220)                 — GST rate/type/GSTIN extraction
│   ├── tally_group_engine.rs (314)         — Tally chart-of-accounts grouping
│   ├── narration_cleaner.rs (524)          — narration normalization
│   ├── settings.rs (98)                    — settings/API-key persistence
│   ├── auth/{mod.rs, monthly_password.rs}  — 159 lines, licensing gate
│   ├── db/mod.rs (698)                     — schema + all SQL
│   ├── export/{mod,tally,excel,accounting}.rs — 1102 lines, 3 export formats
│   ├── ui/mod.rs (161)                     — AppState (Rust↔Slint state mirror)
│   ├── bin/pdf_diag.rs (58)                — standalone PDF diagnostic CLI
│   └── parser/ (13 files, ~7700 lines)     — bank_detection, pdf_parser, excel_parser,
│                                              ocr_parser, ocr_extractor, column_detector,
│                                              transaction_extractor, amount_parser,
│                                              date_parser, noise_filter, row_builder,
│                                              party_master, text_extractor, mod.rs
└── ui/ (4 Slint files, 4269 lines)
    ├── app.slint (458)      — root window, state container, Login/Main switch
    ├── login.slint (159)    — login screen
    ├── dashboard.slint (788)— 7 reusable chart/card components + DashboardScreen
    └── main_screen.slint (2864) — the app: transaction table + 15 modals
```

**Totals:** 35 Rust source files / ~21,178 lines; 4 Slint files / 4,269 lines. **No `tests/` integration-test directory. No `migrations/` directory.** 40 git commits, single contributor.

### 1.2 Module inventory (purpose / status / completion / risk)

| Module | Purpose | Status | Completion | Risk |
|---|---|---|---|---|
| `src/main.rs` | Orchestrates every UI callback, owns `Arc<Mutex<AppState>>`/DB connection | Live, central | ~90% functional / poor structural | **HIGH** (2,730-line `main()`, silent DB-write failures) |
| `src/db/mod.rs` | Schema + all queries | Live | 85% | **HIGH** (no migrations, no bulk-tx wrapping, dup-rule bug) |
| `src/auth/*` | Monthly licensing password | Live, works as designed | 100% of intended scope | **CRITICAL** (not real access control) |
| `src/settings.rs` | Settings + AI key storage | Live | 90% | **HIGH** (plaintext secret storage) |
| `src/parser/*` (13 files) | PDF/Excel/OCR ingestion pipeline | Live, mostly wired | 80% (CSV txn import missing) | MEDIUM |
| `src/classifier.rs` | Rule-based classify + dedup | Live | 75% (1 of 3 documented dedup passes is dead) | MEDIUM-HIGH |
| `src/ai_classifier.rs` | Multi-provider LLM classify | Live, feature+consent-gated | 90% | MEDIUM (3rd-party PII transmission, but genuinely consented) |
| `src/gst_engine.rs` | GST rate/type/GSTIN extraction | Computed, mostly discarded downstream | ~40% effective | **HIGH** (silent data loss) |
| `src/tally_group_engine.rs` | 20-group Tally classifier | Live, 7/20 groups unreachable | 65% | MEDIUM |
| `src/narration_cleaner.rs` | Narration clean + UTR/type extraction | Live, 2 of 4 outputs discarded | 60% effective | MEDIUM |
| `src/analytics.rs` | Dashboard aggregates | Live | 85% | MEDIUM (zero tests) |
| `src/export/tally.rs` | Tally XML (TDML) export | Live | 80% (no GST breakup; `gstin`/`fy` opts unused) | MEDIUM-HIGH |
| `src/export/excel.rs` | Excel/CSV export | Live | 80% | MEDIUM |
| `src/export/accounting.rs` | Zoho/QuickBooks/Odoo/CSV/XML export + validation | Live | 80% (`state_code`/`include_gst` unused) | MEDIUM-HIGH |
| `src/ui/mod.rs` | AppState struct | Live | 90% | LOW |
| `ui/*.slint` | UI layer | Live, visually complete | 85% visual / ~55% accessibility+responsiveness | MEDIUM-HIGH |
| `src/bin/pdf_diag.rs` | Dev diagnostic tool | Standalone utility | 100% | LOW |

### 1.3 Dependency inventory (from `Cargo.toml`)

| Crate | Purpose | Notes |
|---|---|---|
| `slint` 1.7 (optional) | UI framework | feature-gated as `slint-ui` |
| `rfd` 0.15 (optional) | Native file dialogs | |
| `rusqlite` 0.31 (bundled) | SQLite | no encryption variant enabled |
| `hmac`, `sha2`, `base64` | Auth HMAC | auth-only, **not** data-at-rest crypto |
| `chrono`, `serde`/`serde_json` | Date/serialization | |
| `anyhow`, `thiserror` | Error handling | **`thiserror` declared but never imported anywhere in `src/`** — dead dependency |
| `log`, `env_logger` | Logging | |
| `regex`, `once_cell` | Pattern matching | |
| `tokio` (rt, rt-multi-thread, macros, sync, time) | Async runtime | **zero `tokio::` usage anywhere in `src/` — fully dead dependency** |
| `reqwest` (json, blocking, optional `ai` feature) | AI HTTP calls | used, blocking client only |
| `quick-xml` | Tally XML | |
| `csv` | CSV read/write | used only for **ledger** import, not bank-statement transactions |
| `calamine` | Excel read | |
| `umya-spreadsheet` (optional `excel-export`), `rust_xlsxwriter` | Excel write | |
| `lopdf` | PDF text extraction | |
| `rayon` | Parallel iteration | **zero `par_iter`/`rayon::` usage anywhere — fully dead dependency** |

### 1.4 Database inventory

8 tables, all in `src/db/mod.rs` (`SCHEMA_SQL`, lines 576-676): `audit_log`, `clients`, `transactions`, `ledgers`, `classification_rules`, `import_history`, `settings`, `dedupe_hashes`. Foreign keys enforced (`PRAGMA foreign_keys = ON`, line 546-547), cascade deletes configured on `transactions` and 4 related tables. Full schema/constraint/index detail in Phase 6.

### 1.5 UI screen inventory

`LoginScreen` (`ui/login.slint`) → `MainScreen` (`ui/main_screen.slint`, 2864 lines: transaction table + Dashboard tab embedding `DashboardScreen` components from `ui/dashboard.slint` + 15 modal dialogs: add-txn, ai-consent, ai-settings, audit-trail, batch-monitor, edit-client, edit-txn, excel-export, export-wizard, import-history, new-client, reconcile, settings, tally-export, view-rules).

### 1.6 Service / Parser / Import-Export inventory

Covered in full in Phases 3, 5, and 6 below with wiring verdicts per item — not duplicated here to avoid repetition.

---

## Phase 2 — Gap Analysis

| # | Item | Classification | Status | Evidence |
|---|---|---|---|---|
| 1 | Monthly-password auth as real access control | **CRITICAL** | Implemented but not fit for purpose | `src/auth/monthly_password.rs:18-31` hardcoded secret |
| 2 | Data-at-rest encryption | **CRITICAL** | Missing | No crypto crate beyond auth-only HMAC/SHA2; `db/mod.rs:540` plain `Connection::open` |
| 3 | GST engine output reaching exports | **CRITICAL** | Partially-wired bug | `gst_engine.rs:117` fields never read past `classifier.rs:97` tag-push |
| 4 | Export wizard GSTIN/FY/state-code/include-GST fields | **CRITICAL** | Partially-wired bug | `TallyOpts`/`AccountingOpts` fields populated, never read in `export/tally.rs`/`export/accounting.rs` |
| 5 | Schema migration framework | **HIGH** | Missing | No `user_version`, swallowed `ALTER` errors (`db/mod.rs:560-563`) |
| 6 | Silent DB-write-failure swallowing | **HIGH** | Present, ~19 sites | `main.rs:925,1298,1446,939,2489,2494,2776,3454,3194,3494-3495` |
| 7 | CSV bank-statement transaction import | **HIGH** | Missing | File filter `main.rs:1107` excludes `.csv`; CSV only used for ledger import |
| 8 | OCR progress overlay wiring | **HIGH** | Built, never driven | `main_screen.slint:2745-2773` properties (`ocr-visible/ocr-msg/ocr-pct`) — zero `.set_` calls in `main.rs` |
| 9 | Accessibility (`accessible-*`) | **HIGH** | Missing entirely | Zero matches across all 4 `.slint` files |
| 10 | AI API key plaintext storage | **HIGH** | Present | `settings.rs:8,83` → `settings` table, no encryption |
| 11 | tokio / rayon unused dependencies | **MEDIUM** | Dead weight | Zero call sites in `src/` for either |
| 12 | Tally group engine: 7 of 20 groups unreachable | **MEDIUM** | Partial | `tally_group_engine.rs:26-32` constants, no `KEYWORD_MAP` entries route to them |
| 13 | Narration cleaner: `payment_ref`/`txn_type` discarded | **MEDIUM** | Partially-wired bug | `narration_cleaner.rs:44-48`, only `.cleaned` read (`main.rs:804`) |
| 14 | Dedup hash collision risk (balance excluded, 32-bit) | **MEDIUM** | Design limitation | `parser/mod.rs` `Transaction::hash()` (lines ~168-181) |
| 15 | Classifier's documented "3-pass" dedup is 2 passes | **MEDIUM** | Dead code / doc drift | `classifier.rs:457` docstring vs. `_narr_similarity` at line 494, zero callers |
| 16 | Whole-statement opening/closing balance check | **MEDIUM** | Missing | Only per-row running-balance check exists (`excel_parser.rs:422-471`) |
| 17 | OCR path skips `validate_balances`/`deduplicate_txns` | **MEDIUM** | Missing | `ocr_parser.rs:236` only calls `compute_prev_balances` |
| 18 | `classification_rules` duplicate-rule bug | **MEDIUM** | Latent bug | No `UNIQUE` constraint, `INSERT OR IGNORE` (`db/mod.rs:351-361`) is a no-op |
| 19 | `main()` is 2,730 lines | **MEDIUM** | Maintainability debt | `main.rs:991-3720` |
| 20 | 4-way duplicated balance/tolerance logic across parsers | **MEDIUM** | Duplication, 1 divergence bug | OCR variant missing `.max(1.0)` floor (`ocr_parser.rs:184,186`) |
| 21 | 15 hand-rolled Slint modals, no shared `ModalShell` | **MEDIUM** | Duplication | `main_screen.slint`, backdrop/centering boilerplate ×15 |
| 22 | No keyboard nav / Esc-close on modals / shortcuts | **MEDIUM** | Missing | Confirmed across all 15 modals |
| 23 | Multi-user / role-based access | **MEDIUM** | Missing | No user table; single shared license gate only |
| 24 | No integration tests / no real file fixtures | **MEDIUM** | Missing | `assets/` empty but `.gitkeep`; all 337 tests use inline literals, not real file I/O |
| 25 | "Run Reconciliation" button permanently disabled | **LOW-MEDIUM** | Dead UI | `main_screen.slint:2363`, logic actually fires elsewhere |
| 26 | `delete_transactions_for_client` unused | **LOW** | Confirmed harmless — cascade FK delete already covers this | `db/mod.rs:267` vs. live `delete_client` (`db/mod.rs:98`) |
| 27 | `match_bank()`, `_narr_similarity`, `tally_date`, `display_to_ts`, `fmt_amount`, `format_indian*`, `get_client` | **LOW** | Confirmed dead code | Compiler-flagged, grep-confirmed zero external callers |
| 28 | Repo hygiene: committed `.patch`/`.diff` backup files, stale `PARITY_GAP_REPORT.md` | **LOW** | Debt | Repo root |

---

## Phase 3 — Bank Statement Processing Capability Audit

| Capability | Status | Evidence |
|---|---|---|
| PDF statements (text-based) | **Production-ready** | `pdf_parser::parse_pdf_rows`, multi-stage fallback (`pdf_parser.rs:94-117`) |
| PDF statements (password-protected) | **Production-ready** | Real `lopdf::Document::decrypt` path, `text_extractor.rs:35-39,112-164`, UI password prompt |
| PDF statements (scanned/image) | **Production-ready**, external dependency | Falls to Tesseract OCR when `lopdf` extracts no text (`main.rs:1177-1193`) |
| Excel statements (.xlsx/.xls/.xlsm) | **Production-ready** | `excel_parser::parse_excel_file`; gap: doesn't call bank-detection (item below) |
| CSV statements (bank transactions) | **MISSING** | No `.csv` in import file filter (`main.rs:1107`); CSV path exists only for ledger-master import |
| Image files (PNG/JPG/TIFF/BMP) via OCR | **Production-ready**, external dependency | `ocr_extractor.rs` shells out to system Tesseract CLI; graceful degrade if not installed (`main.rs:1185,1246`) |
| Multi-bank imports | **Production-ready** | Batch import aggregates distinct banks across files (`main.rs:1469-1472`) |
| Auto bank detection | **Production-ready**, one gap | 45 IFSC prefixes + 42 phrase patterns + 19 regexes + 20 OCR-fuzzy abbreviations, confidence-scored (`bank_detection.rs:18-157,470-553`); **not invoked from `excel_parser.rs`**, so Excel-sourced transactions get a blank `bank_name` |
| OCR | **Production-ready**, fragile dependency model | Binary shell-out, not a bundled library — Tesseract must be separately installed on the target machine |
| Transaction extraction | **Production-ready** | Format-agnostic column/header detection, handles merged PDF cells, combined Dr/Cr columns, multi-line narrations (`column_detector.rs`) |
| Transaction validation | **Partial** | Per-row running-balance check exists for Excel/PDF (advisory only, never blocks import); **missing for OCR path**; no whole-statement opening/closing reconciliation |
| Duplicate detection | **Production-ready, with caveats** | Two mechanisms: DB-persisted cross-import hash dedup (32-bit, balance excluded from hash — real false-positive risk) + 2-of-3-documented-passes in-batch advisory dedup |
| Reconciliation | **Partial / confusing UX** | Bank-vs-Tally reconciliation logic runs on "Import Tally Export," but the modal's own "Run Reconciliation" button is permanently disabled (`main_screen.slint:2363`) |
| Categorization | **Production-ready, with a major hidden gap** | Rule-based + AI (3 providers) + Tally-group classification all live; GST-specific categorization computed but not surfaced (Phase 2 #3) |
| Audit trail | **Production-ready** | `audit_log` table, `push_audit_event`, single-level undo, all DB-persisted (per recent commit history) |
| User management | **MISSING (real multi-user)** | One shared monthly-password gate; no accounts, roles, or permissions |
| Reporting | **Partial** | Dashboard (`analytics.rs` + `dashboard.slint`) provides real-time stat cards, monthly/expense/cashflow/vendor charts with drill-down filters — genuine BI-style reporting. No formal printable financial statements (P&L/ledger format) beyond raw data export. |
| Exporting | **Production-ready structurally, incomplete on tax data** | Tally XML (TDML), Excel, and Zoho/QuickBooks/Odoo/CSV/XML (`export/accounting.rs`) all generate real, structurally valid files; none include a populated GST/tax breakup or running balance column |

### Missing-capability deep dives

**CSV bank-statement import** — *Why it matters:* many statements (especially from smaller/regional banks, or statements already exported once) arrive as CSV; the current import dialog cannot open them as transactions at all. *Effort:* **Low (3-5 days).** *Architecture:* new `src/parser/csv_parser.rs` reusing `column_detector`'s existing header/content-detection logic against CSV-parsed rows (the `csv` crate is already a dependency); add `.csv` to the file-open filter and an extension-routing branch in `main.rs`'s import handler.

**Real multi-user access control** — *Why it matters:* an accounting firm audience implies multiple staff with different trust levels touching the same client data; a single shared monthly password provides no accountability or segregation. *Effort:* **High (4-6 weeks).** *Architecture:* a `users` table (id, username, password hash via `argon2`, role), a session/login rework replacing the current `LoginScreen`, role-gated UI actions (delete client/transaction, rules edit, export), and audit-log entries tied to a real user id instead of being anonymous.

**Data-at-rest encryption** — *Why it matters:* the DB holds real bank account numbers, balances, and full narrations in plaintext; any filesystem-level compromise (malware, stolen laptop, backup leak) exposes everything with zero additional effort. *Effort:* **High (2-4 weeks)** including a migration path for existing unencrypted `.db` files. *Architecture:* swap to an SQLCipher-enabled `rusqlite` build (or add an app-level encryption layer for sensitive columns), add passphrase/key management UI, write a one-time migration tool for existing databases.

**Schema migration framework** — *Why it matters:* every future schema change currently relies on best-effort, error-swallowed `ALTER TABLE` statements; a destructive change will fail silently and desync the running app from its own DB. *Effort:* **Medium (1 week).** *Architecture:* `PRAGMA user_version` + an ordered `Vec<(u32, &str)>` of migration SQL applied sequentially at `db::open()`, replacing the current best-effort block.

---

## Phase 4 — Rust Engineering Review

**Panic risk:** the raw counts (240 `.unwrap()`, 41 `.expect()`, 0 `unsafe`, 1 `panic!`) look alarming in isolation but are **mostly safe by construction** on inspection:
- ~115 of `main.rs`'s 120 unwraps are `state_ref.lock().unwrap()` — panics only on a poisoned mutex (i.e., only after some *other* thread already panicked while holding the lock). With one background thread in the whole app (AI classify), this surface is narrow.
- Parser-file unwraps are either compiled-from-literal regexes (safe) or `Option::unwrap()` immediately guarded by an `is_some()`/`is_empty()` check a few lines earlier — verified safe at `excel_parser.rs:271,360,438,493,773,778`; `column_detector.rs:529,609,773,777`; `transaction_extractor.rs:243,320,330,336,460,505,508,515,529`; `pdf_parser.rs:297,366,444,465`; `ocr_parser.rs:80,146`. These are **brittle, not unsafe** — a future refactor that reorders the guard would silently reintroduce a panic.
- Most `.expect()` calls live inside `#[cfg(test)]` blocks — zero production risk.
- `src/export/*.rs` is the panic-safety model for the rest of the codebase: **zero bare `.unwrap()` anywhere**, every fallible value goes through `.unwrap_or(...)`/`.unwrap_or_default()`.

No CRITICAL/catastrophic-on-malformed-input panic was found. **The real risk in this codebase is silent data-integrity failure, not crashes:**

- **`thiserror` is a declared dependency but never used anywhere in `src/`** — all error handling is `anyhow`, with no domain-specific error types for callers to match on.
- **~19 sites silently swallow DB write errors** via `let _ = db::...`, including `main.rs:925` (transaction persistence — followed unconditionally by a `log::info!("...persisted...")` regardless of whether the write actually succeeded) and `main.rs:3494-3495` (delete-transaction — the UI removes the row from memory *before* the DB delete, so a failed DB delete makes the row silently reappear on next reload with zero error shown). Also at `main.rs:1298,1446,939,2489,2494,2776,3454,3194`.
- Batch-mode parse failures are swallowed without logging the cause (`main.rs:1349,1351` — `.ok()` discards the actual `anyhow::Error`), so an operator sees "N errors" with no indication of why.

**Concurrency/async:** `tokio` and `rayon` are both **fully dead dependencies** (zero call sites). File parsing and OCR run **synchronously inside the Slint UI callback** with no thread offload — `parser::ocr_extractor::extract_via_tesseract` blocks on an external process for potentially seconds per page with the UI thread frozen and no progress feedback (compounding the Phase 5 finding that the OCR progress overlay is also never driven). The one correct pattern in the app is AI classification (`main.rs:1627`, real `std::thread::spawn` + `slint::invoke_from_event_loop` marshaling) — this pattern should be extended to file parsing, which is arguably more user-visible.

**Performance:** no O(n²) algorithm found in dedup/classification hot paths (the one that would be, `_narr_similarity`, is dead code). `classifier::apply_rules` re-uppercases every rule pattern on every transaction (`classifier.rs:36-38`) — an avoidable O(N×M) allocation. `compute_filter_counts` (`main.rs:357-367`) does 6 separate full-vector scans on essentially every UI mutation. The "derive opening balance"/2%-tolerance logic is independently reimplemented in 3 of 4 parser backends rather than reusing the existing shared helper, and the OCR copy diverges (missing a `.max(1.0)` floor, `ocr_parser.rs:184,186`) — a genuine correctness inconsistency, not just duplication.

**Code smells:** `main()` is **2,730 lines** (`main.rs:991-3720`), the dominant maintainability risk in the codebase — any change anywhere in it risks lock-ordering/borrow regressions that are hard to localize. `column_detector.rs` (1,537 lines) is the second-largest file. Magic numbers (`0.02`, `0.6`/`0.9`/`0.45` confidence tiers, narration-truncation lengths) are repeated as literals rather than named constants throughout.

---

## Phase 5 — Slint UI Review

**Architecture:** `app.slint` (root/state container) → `login.slint` / `main_screen.slint` → `main_screen.slint` imports chart components from `dashboard.slint`. Clean, non-circular include graph. Real component reuse exists at the "atom" level — `dashboard.slint`'s 7 chart/card components and `main_screen.slint`'s 12 button/row/modal-frame helpers are genuinely shared and instantiated repeatedly.

**Above the atom level, `main_screen.slint` is a monolith.** All **15 distinct modal dialogs** hand-roll their own backdrop, centering math, and header/footer wrapper rather than sharing one `ModalShell` component — a single visual tweak to "all dialogs" currently requires 15 separate edits. Three hand-rolled tables (transactions, rules, import history/audit) exist with no shared base table component either.

**Wiring audit — the important part.** All 45 app-level callbacks declared in `app.slint` have a matching `on_do_*` registration in `main.rs` (45/45 verified). Of 12 business-critical flows traced individually, 10 are confirmed live and correct (import, AI classify, export preview/generation, edit/delete transaction, mark-suspense, rule CRUD, dedup toggle, single-level undo, multi-status filter toggle). Two are broken:
- **CRITICAL — OCR progress overlay is fully built and never driven.** `main_screen.slint:2745-2773` defines a complete overlay bound to `ocr-visible`/`ocr-msg`/`ocr-pct`; grepping `main.rs` for any of these three property names returns **zero matches**, despite real, synchronous, potentially multi-second OCR work running at `main.rs:1181,1229,1357-1370,3630-3637`. Users get no feedback and the UI appears frozen during what is likely the slowest operation in the app.
- **LOW-MEDIUM — "Run Reconciliation" button is permanently `enabled: false`** (`main_screen.slint:2363`) with no click handler; the actual reconcile logic fires from a different button in the same modal, leaving a dead control that will confuse anyone reading the dialog literally.

**Accessibility: zero `accessible-*` properties anywhere across all 4 `.slint` files.** No keyboard navigation beyond Enter-to-submit on the login screen; **no Esc-to-close on any of the 15 modals**; no shortcuts (Ctrl+S/Ctrl+Z/Ctrl+F, etc.). For software aimed at back-office accounting staff, this is a real production gap, not a nice-to-have.

**Responsiveness:** `dashboard.slint` is genuinely adaptive (stretch-based layout). `main_screen.slint`'s transaction table uses fixed-pixel column widths (~1,186px total, only the narration column stretches) that will clip rather than reflow on narrower windows or different DPI; all 15 modals are fixed-size and centered, not content-adaptive.

**UX completeness:** single-level undo only (despite an in-memory stack that could trivially support more), no redo, no multi-client tabs, no print/PDF preview of exports, no dark mode/theming, and the error toast has no auto-dismiss timer (persists indefinitely until overwritten).

---

## Phase 6 — Database Review

**Schema** (`src/db/mod.rs:576-676`, 8 tables): foreign keys correctly enforced (`PRAGMA foreign_keys = ON`, every connection open). Good index coverage on the hot `transactions` table (`idx_txn_client`, `idx_txn_date_ts`, `idx_txn_status`, `idx_txn_vendor`). Two real defects: **`classification_rules` has no `UNIQUE` constraint**, so `add_rule`'s `INSERT OR IGNORE` (`db/mod.rs:351-361`) is a no-op and duplicate rules silently accumulate; **`import_history.file_hash`** is declared but never queried by `save_import` — a vestigial column that looks like dedup support but does nothing.

**Scale risk:** `upsert_transactions` and `add_dedupe_hashes` execute **one `INSERT` per row in a loop with no explicit `conn.transaction()` wrapper** (`db/mod.rs:128-167,481-489`) — for a multi-thousand-row statement this means thousands of individual implicit-autocommit writes. This is the single biggest "falls over at scale" finding in the database layer and the cheapest to fix.

**SQL injection: clean.** Exhaustive check — `db/mod.rs` is the only file in `src/` issuing SQL, and every one of its ~30 query sites uses `rusqlite::params![]` placeholders. No string-interpolated SQL anywhere.

**Migration/versioning: high risk.** No `PRAGMA user_version`, no migrations table. Schema evolution relies on `CREATE TABLE IF NOT EXISTS` plus a single best-effort `ALTER TABLE ... ADD COLUMN` whose error is explicitly discarded (`let _ =`, `db/mod.rs:560-563`). This has worked for the two migrations done so far (both additive) but has no audit trail and will fail silently and undetectably the first time a genuinely destructive schema change ships.

---

## Phase 7 — Security Review

**Authentication: CRITICAL as an access-control boundary.** `src/auth/monthly_password.rs` computes `HMAC-SHA512(secret_key, "<email>|YYYY-MM")`, formatted into a license-key-style string. The secret key is hardcoded in source, split into fragments purely as cosmetic obfuscation (`SK_FRAGMENTS`, lines 18-27, reassembled deterministically by `secret_key()`). **Anyone with the binary or source can compute a valid password for any email/month offline.** No per-user identity, no password storage, no lockout/rate-limiting. This mirrors a pre-existing weakness in the original Electron app (per an in-code comment) — it is adequate as a monthly-license nag, never as the thing protecting real client banking data.

**Secrets handling:** no hardcoded API keys or third-party tokens found beyond the auth secret above. The user-supplied AI provider key is stored **as plaintext** in the `settings` key-value table (`settings.rs:8,83` → `db/mod.rs:663-667`) — anyone with filesystem read access to the `.db` file can read it directly.

**AI data transmission — verified and partly corrected from an initial overstated finding.** `ai_classifier.rs` sends only `narration` text (`ai_classifier.rs:100-102,157-163`) to whichever provider is configured — **not** amounts, vendor names, account numbers, or balances, despite the consent modal's copy claiming amounts and vendor names are also transmitted (`main_screen.slint:2678`) — a disclosure-accuracy bug in the conservative direction, not a privacy violation. **There is a real, substantive, mandatory consent gate** (`main_screen.slint:2636-2710`) with an accurate "account numbers/IFSC/bank names/balances are NOT transmitted" claim — this directly contradicts `PARITY_GAP_REPORT.md`'s stale claim that no such modal exists. Gemini's API key appears in the request URL query string (`ai_classifier.rs:253-255`) — this is how Google's documented REST API works, not an implementation bug in this app, but it is a marginally higher-risk transport (more likely to be logged by intermediate proxies) worth knowing about.

**Data-at-rest: HIGH.** No SQLCipher, no AES, no encryption of any kind on the SQLite file holding real account numbers, narrations, and balances (confirmed via `Cargo.toml` — only auth-only `hmac`/`sha2`/`base64` exist).

**Input validation / file handling:** file selection goes through the native OS picker (no path-traversal vector). Row/column/amount-magnitude caps exist (`excel_parser.rs:37-38`, `amount_parser.rs:51`) as reasonable DoS mitigation against huge malicious spreadsheets. Residual risk: scattered `.unwrap()`s on `Option`-returning balance fields reachable from real parsing logic (e.g. `excel_parser.rs:360,438,493,779`) could crash on an unusual statement layout — a crash-DoS on a single malformed file, not a memory-safety issue given Rust's guarantees.

---

## Phase 8 — Testing Review

`cargo test --lib --no-default-features`: **330 passed, 0 failed.** This does not cover all 337 `#[test]` functions that exist in the repo — `src/lib.rs` omits the `auth`/`ui`/`settings`/`ai_classifier` modules (they're only `mod`-declared in the bin crate), so 7 tests (5 in `auth/monthly_password.rs`, 2 in `ui/mod.rs`) are excluded from this run. **`cargo test --bin ... --no-default-features` fails to compile** (17 errors — unresolved Slint types/helper functions) — meaning **there is currently no single command that runs all 337 tests successfully.**

**Test quality is genuinely good where it exists** — sampled tests in `column_detector.rs`, `bank_detection.rs`, and `date_parser.rs` assert real bank-specific values (exact column indices for HDFC/SBI headers, confidence scores, calendar edge cases like Feb 30/Apr 31), not trivial happy-path-only or panic-only checks. The hash-distinctness tests explicitly admit (comment, `parser/mod.rs` test module) they cannot verify the hash matches the original JS algorithm and that **no collision test exists at all.**

**Zero tests** in: `main.rs` (the entire 3,720-line orchestration layer), `ai_classifier.rs`, `export/tally.rs`, `export/excel.rs`, `export/accounting.rs`, `analytics.rs`, `settings.rs`.

**No integration tests, no real file fixtures anywhere** (`assets/` contains only `.gitkeep`). Critically — even `excel_parser.rs`'s 30 tests never call its own `parse_excel_file` entry point; every parser test (Excel, PDF, OCR) feeds hand-built in-memory structs/literal strings, never real bytes through the actual `calamine`/`lopdf`/Tesseract pipeline. **Unit tests passing proves internal logic is self-consistent — it proves nothing about whether a real HDFC/SBI PDF or password-protected Excel file from production actually reaches that logic correctly.**

**Highest-value missing test cases** (concrete, not generic): (1) `parse_excel_file` end-to-end against a real written `.xlsx` temp file; (2) Tally XML well-formedness via an actual XML parser on the output string; (3) GST figures in exported files asserted against known input amounts (currently `gst_tagged` is only counted, never value-verified); (4) dedup-against-DB hash-collision behavior across re-imported overlapping periods; (5) re-import idempotency through the full pipeline; (6) `parse_excel_file` error paths (corrupt/password-protected/empty file); (7) a real PDF read through `lopdf` rather than hand-built `PdfItem`s; (8) `ai_classifier` failure modes (timeout, malformed JSON, rate-limit) asserting graceful fallback; (9) `analytics.rs` aggregate correctness against hand-calculated totals (guards the recently-fixed "live dashboard counters" bug from recurring); (10) `export_xlsx` round-trip (write then re-read with calamine).

---

## Phase 9 — Production Readiness Scoring

| Category | Score /100 | Rationale |
|---|---|---|
| Architecture | 55 | Reasonable module boundaries (parser/export/db/auth separated); undermined by a 2,730-line `main()` and no migration framework |
| Code Quality | 55 | Strong panic-guard discipline, clean export module; undermined by ~19 silently-swallowed DB errors, dead `tokio`/`rayon`, duplicated tolerance logic with a real divergence bug |
| Security | 25 | SQL injection is genuinely clean (rare strength) and AI consent is real; outweighed by forgeable auth, plaintext secrets, zero data-at-rest encryption |
| Performance | 45 | No O(n²) found, decent indexes; per-row (non-transactional) bulk inserts and UI-thread-blocking synchronous parsing/OCR will not hold up at stated scale |
| Scalability | 30 | Single-user, single-machine SQLite desktop design with no migration path and synchronous parsing — "millions of transactions in production" is aspirational, not current |
| Maintainability | 42 | 2,730-line `main()`, 15× copy-pasted modal boilerplate, 4-way duplicated balance logic |
| Testing | 48 | 330 genuinely good unit tests; zero integration tests, zero real-file fixtures, several core modules (main.rs, all 3 exporters, AI classifier, analytics) completely untested |
| UX | 50 | Strong visual parity and error-toast coverage; zero accessibility, no keyboard nav, OCR progress silently never shown |
| Operations | 32 | No migration framework, no crash telemetry, no CI evidence in-repo, no installer/update story audited |

**Weighted overall: 42 / 100.** Read as: a working prototype with a genuinely strong parsing/classification core, not an enterprise-ready system. The gap to "production-ready for accounting firms processing millions of transactions" is dominated by security (auth, encryption) and operability (migrations, scale-safe DB writes, async parsing) — not by the core domain logic, which is the best-tested and best-guarded part of the codebase.

---

## Phase 10 — Implementation Roadmap

### Immediate fixes (days, not weeks)

1. Wrap `upsert_transactions`/`add_dedupe_hashes` loops in `conn.transaction()` — `db/mod.rs:128-167,481-489`.
2. Stop silently swallowing DB write errors in `main.rs`; surface failures as a toast at minimum (`main.rs:925,1298,1446,939,2489,2494,2776,3454,3194,3494-3495`).
3. Add a `UNIQUE` constraint (or pre-check) on `classification_rules` so `INSERT OR IGNORE` actually does something (`db/mod.rs:351-361,639-648`).
4. Wire `TallyOpts.gstin/fy` and `AccountingOpts.state_code/include_gst` into the actual export generators.
5. Wire `ocr-visible`/`ocr-msg`/`ocr-pct` from `main.rs` during OCR/PDF/image parsing (`main_screen.slint:2745-2773`).
6. Fix or remove the dead "Run Reconciliation" button (`main_screen.slint:2363`).
7. Fix the OCR balance-tolerance divergence — add the missing `.max(1.0)` floor (`ocr_parser.rs:184,186`).
8. Remove `tokio`/`rayon` from `Cargo.toml`, or commit to actually using one (see roadmap below).
9. Remove `parity_audit_backup.patch`/`.patch_a_ai_classifier.diff` from version control; retire or regenerate `PARITY_GAP_REPORT.md`.
10. Resolve the dedup docstring/code mismatch — either implement `_narr_similarity` as the documented pass 3, or delete it and fix the comment (`classifier.rs:457,494`).
11. Add a toast auto-dismiss timer (`main_screen.slint:2847-2863`).
12. Correct the AI consent modal copy to match actual transmitted fields (`main_screen.slint:2678`).

### Next 30 tasks (1-4 weeks each, grouped)

**GST/Tally correctness (5):** surface `GstAnalysis.gst_rate/gst_amount/gst_type/gstins/confidence` into the transaction record + UI + exports · add CGST/SGST/IGST columns to Tally XML and accounting CSV templates · add keyword routing for the 7 orphaned Tally groups (Provisions, Reserves, Misc Expenses, Deposits, Loans & Advances, Current Assets, Current Liabilities) · surface `payment_ref` (UTR/cheque ref) into exports for reconciliation · add an end-to-end test asserting exported GST figures are numerically correct.

**Parser/import completeness (5):** add CSV bank-statement transaction import (`src/parser/csv_parser.rs`) · call `bank_detection::detect()` from `excel_parser.rs` · call `validate_balances`/`deduplicate_txns` from `ocr_parser.rs` · add a whole-statement opening+credits-debits=closing check · strengthen the dedup hash (include balance, move to a stronger digest).

**Security/compliance (5):** move the AI API key out of plaintext storage (OS keychain via the `keyring` crate, or app-level encryption) · add SQLCipher (or equivalent) for the SQLite DB · explicitly scope the monthly-password mechanism as licensing-only in docs/UI and design the real auth layer that should sit in front of it · audit-log every destructive action tied to a real identity once multi-user auth lands · add a `SECURITY.md`.

**Testing (5):** build a small fixture library of realistic synthetic PDF/Excel/CSV statements for 5-6 common banks under `tests/fixtures/` · add integration tests against `parse_excel_file`/`parse_pdf_rows` using those fixtures · add a full pipeline test (import → dedup → classify → export) asserting final file content · add tests for all 3 exporters (currently zero) · fix the test-build gap so one command runs all 337 tests, including `auth`/`ui`.

**Architecture/maintainability (5):** break up `main()`'s 2,730 lines into per-feature handler modules · extract the 4×-duplicated balance/tolerance logic into one shared helper · introduce a `PRAGMA user_version` migration framework · extract a shared Slint `ModalShell` component · precompute rule-pattern uppercasing once instead of per-transaction.

**UX/accessibility (5):** add `accessible-*` roles/labels across all 4 `.slint` files · add Esc-to-close on all 15 modals · add basic keyboard shortcuts (Ctrl+S/Ctrl+Z/Ctrl+F) · make the transaction table's columns responsive instead of fixed-pixel · expose the existing undo stack as multi-step undo/redo in the UI.

### Next 100 tasks (thematic enterprise backlog, ~10 per theme)

1. **Multi-user & RBAC:** user accounts, argon2 password hashing, role model (admin/preparer/reviewer), session management, per-user audit trail, login lockout/rate-limiting, password reset, user activity dashboard, permission gates on destructive actions, SSO/AD integration (stretch).
2. **Data protection & compliance:** SQLCipher integration, key management UI, encrypted backups, data-retention policy, PII-redaction toggle for AI calls, on-prem/local-model AI option, audit-log tamper-evidence (hash chain), secure update channel, responsible-disclosure policy, client data export/erasure workflow.
3. **Scalability & performance:** batch-transaction DB writes, background-thread/async file parsing, paginated transaction table, virtualized Slint list rendering for very large statements, profiling + benchmarks, streaming parsing for huge files, connection pooling (if multi-process), performance budgets/alerts, load testing with a synthetic millions-of-rows dataset, query-plan review at that scale.
4. **Reliability & operations:** opt-in crash telemetry, structured/rotated logging, scheduled DB backup/restore, self-diagnostics screen, installer/auto-updater, CI pipeline (build+test+lint per PR), release versioning/changelog automation, error-code catalog, log-bundle export for support, staging environment with masked data.
5. **Accounting correctness:** whole-statement reconciliation, multi-currency support, FY-aware reporting, GST-return-ready export (GSTR-2B reconciliation), TDS tracking, a true bank-vs-books auto-match reconciliation engine, joint/overdraft account support, credit-card statement support, refined recurring-transaction detection, year-end closing/carry-forward.
6. **UX/Accessibility:** full `accessible-*` pass, keyboard navigation, Esc-close everywhere, dark mode/theming, responsive tables, multi-client tabs, redo, print/PDF export preview, in-app help/tooltips, localization (i18n).
7. **Parser robustness:** CSV import (started in next-30), password-protected Excel, credit-card/loan-specific parsers, bank coverage beyond the current 45, OCR accuracy tuning, ML fallback for irregular PDF tables, regional-language statement support, corrupted-file recovery mode, configurable column-mapping override UI, a parser plugin architecture.
8. **Classification intelligence:** full GST wiring (extends next-30), cross-client vendor-master learning, rule-conflict detection UI, a local (non-cloud) ML classifier option, confidence-threshold tuning UI, classification explainability, bulk re-classify with diff preview, rule versioning/rollback, industry-specific rule templates, an accuracy-metrics dashboard.
9. **Testing & QA:** fixture library (extends next-30), property-based testing for parsers, fuzz testing on file parsers, mutation testing for classifier rules, a load/perf test suite, Slint UI snapshot tests, a CI security-regression suite, golden-file tests per export format, cross-platform build verification, automated accessibility checks.
10. **Documentation & enablement:** architecture decision records, contributor onboarding doc, end-user manual, admin guide (once multi-user lands), export data-dictionary, known-limitations doc, support runbook, a security/compliance whitepaper for client due-diligence, a video walkthrough, an Electron→Rust migration guide.

### Enterprise-grade roadmap (phased)

- **Weeks 1-2:** all Immediate fixes + AI-consent copy correction.
- **Weeks 3-6:** GST/Tally correctness, CSV import, fixture-based testing, migration framework.
- **Months 2-3:** data-at-rest encryption, real multi-user auth/RBAC design and implementation.
- **Months 3-5:** scalability work (batch DB writes, async parsing, pagination, virtualized rendering), true reconciliation engine, GST-return-ready export.
- **Months 5-8:** accessibility/UX overhaul, operations maturity (CI/CD, telemetry, installer/updater), classification-intelligence upgrades.
- **Ongoing:** testing depth, documentation, localization, parser coverage expansion.

### Production release checklist

- [ ] All Immediate fixes merged
- [ ] Data-at-rest encryption shipped, or risk formally accepted in writing by deploying firms
- [ ] Real per-user auth in front of the monthly-password gate, or a documented, signed-off exception
- [ ] Migration framework in place with a tested upgrade path from the current schema
- [ ] GST/Tally export fields verified correct by an actual accountant against a real Tally import
- [ ] CSV bank-statement import shipped, or explicitly descoped and communicated
- [ ] Full 337-test suite runs green in one CI command; fixture-based integration tests added
- [ ] Accessibility pass complete (`accessible-*` labels, keyboard nav, Esc-close)
- [ ] Load test against a synthetic millions-of-transactions dataset meets a defined performance budget
- [ ] Crash telemetry + structured logging in place
- [ ] Backup/restore and encryption-key recovery procedure documented and tested
- [ ] External security review/pen-test sign-off for real client banking PII handling
- [ ] `PARITY_GAP_REPORT.md` and committed `.patch`/`.diff` artifacts retired from the repo

---

## Phase 11 — Master Summary

**Current state:** a working, single-developer Rust + Slint desktop application with a genuinely capable, well-tested core parsing pipeline (PDF/Excel/OCR, 45-bank detection, format-agnostic column detection) and a classification layer (rules + AI + Tally grouping + GST analysis) that computes far more than it currently surfaces.

**Missing features:** CSV bank-statement import, real multi-user accounts/RBAC, data-at-rest encryption, a schema migration framework, whole-statement balance reconciliation, formal printable financial reports.

**Technical debt:** a 2,730-line `main()`, 15× copy-pasted Slint modal boilerplate, 4-way duplicated balance/tolerance logic with one real divergence bug, two fully-dead dependencies (`tokio`, `rayon`), a stale and actively misleading `PARITY_GAP_REPORT.md`, committed patch/diff backup artifacts.

**Critical risks:** forgeable authentication, plaintext secrets and unencrypted client banking data at rest, and — the most consequential "looks done but isn't" finding — GST rate/amount/type data and export-wizard tax fields that are computed/collected and then silently discarded before reaching any output file.

**Production blockers (in order of blocking severity):** (1) auth/encryption — the app should not hold real client banking data in its current security posture; (2) the GST/Tally export bugs — a flagship feature an accounting firm would rely on does not do what the UI implies; (3) the migration framework gap — any further schema change is a silent-corruption risk; (4) the lack of integration tests against real file formats — correctness of the actual product, not just its unit-tested internals, is currently unverified.

**Recommended architecture direction:** keep the existing parser/export/db module boundaries (they're sound), but (a) decompose `main.rs` into feature-scoped handler modules, (b) introduce a real migration framework before any further schema work, (c) move long-running file/OCR work off the UI thread using the pattern already proven correct for AI classification, and (d) treat security (auth + encryption) as a dedicated workstream before any "production" claim is made to a client firm.

**Prioritized backlog:** see Phase 10 in full above — Immediate fixes first, then the GST/Tally correctness and security items in the Next-30 tier, with the Next-100 thematic backlog as the multi-quarter enterprise roadmap.
