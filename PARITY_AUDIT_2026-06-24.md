# Feature Parity Audit — Electron (`bank-statement-processing`) vs Rust/Slint Rewrite (`bank-statement-processor-rust`)

**Date:** 2026-06-24
**Auditor scope:** Every screen, dialog, wizard, tab, report, export, workflow, field, filter, setting, and business-logic engine in the old Electron app, verified directly against source (not documentation), then cross-referenced against the current Rust app's source.
**Method:** Full read of `app.js` (3789 lines), `dashboard.js` (339 lines), `parser.js` (2898 lines), `index.html`/`login.html`, all 9 `src/engines/*.js` files, `db.js`/`db-enhanced.js`, `main.js`/`preload.js`, `password_generator.js`, `src/services/*`, `src/workers/batch-processor.js` — cross-referenced against the equivalent Rust modules, UI `.slint` files, and direct constant-for-constant diffing where precision mattered (regexes, thresholds, keyword lists).

## 0. Critical methodology note — every existing doc in both repos is stale

Before reading code, three documents looked like they might shortcut this audit. All three turned out to misrepresent current reality, sometimes badly:

| Document | Claim | Reality |
|---|---|---|
| `docs/product-requirement.md` (old app, v1.0, 2026-05-06) | "All data in localStorage, no backend"; 3-tier classifier; 9-column table; 3 filter buttons; no AI/GST/reconciliation/dashboard | Persistence claim is correct, but the app has since grown an AI classifier, a GST engine, a reconciliation engine, a full dashboard, a settings screen, and an audit trail — **none mentioned**. Table actually has 14 columns and 7 filter buttons. Several documented constants (12-row Excel header scan, 25px PDF merge gap) are simply wrong (actual: 50 rows, 40px). |
| `PRODUCTION_READINESS_REPORT.md` (old app, 2026-06-09) | Lists Rust as missing OCR, settings persistence, AI classify, client edit/delete, etc. | All since fixed in this session before this audit started — stale by two weeks of active Rust development. |
| `PARITY_GAP_REPORT.md` (Rust repo) | Per project memory, already known to go stale fast | Not consulted; superseded by this document. |

**Implication for the rest of this report:** every row in the matrix below was checked against live source on both sides, not against any of the above. Where this audit's findings differ from prior reports, this audit's findings are authoritative because they were re-verified today.

## 1. Headline finding

The Rust rewrite is **far closer to parity than a typical "rewrite gap" assumption predicts**. Several engines are not approximate ports but byte-for-byte faithful translations — e.g. `gst_engine.rs`'s 12-category `VENDOR_GST_MAP` (every keyword, rate, GST type, and expense-ledger string) is an exact match to the Electron `gst-engine.js` original, and `classifier.rs`'s header comment literally states "Ports `App._classify()`... from the original app.js." The parsing pipeline's obscure constants (40px PDF merge gap, 5px row-clustering tolerance, the Kotak `DEBIT/CREDIT(₹)` split-header guard, the Cosmos Co-op fixed-width parser) were independently re-derived from the real `parser.js` source, not from the (incorrect) PRD — confirming the porting work was done against ground truth.

This audit also found that several previously-assumed "gaps" are not gaps at all:
- **CSV transaction import**: the *old app itself* does not support this (`parser.js`'s `parseFile()` dispatch only accepts `.xlsx/.xls/.xlsm/.pdf` + image formats for OCR). A prior report flagged this as a Rust gap relative to a feature the old app never had. Both apps are at parity here — neither supports it.
- **Batch monitor pause/resume/abort**: the old app's advanced `BatchProcessor` (queue, retry, pause/resume/abort, live per-file table) is fully built but **structurally unreachable from its own UI** — `this._batchFiles` is read in two places and assigned in zero, so the "Start" button can never proceed. The actually-used batch path is the simpler sequential `_loadBatch()`, which Rust does match.
- **Tally-group "learning from corrections"**: advertised in the old engine's header comment, but `learnOverride()` has zero call sites anywhere in `app.js` — it never actually learns in production. Not a Rust gap.
- **GST engine's rich per-transaction analysis** (GSTIN list, CGST/SGST split, suggested ledger): computed in the old app but never displayed anywhere in its UI — only a boolean `GST` tag surfaces. Rust matching just the boolean-tag behavior is already at parity; exposing the richer data in either app would be a *new* feature, not a parity fix.

Where Rust **exceeds** the old app (not gaps — improvements, listed so they aren't mistaken for missing-old-app-feature work):
- Real client edit/delete (old app's QA checklist describes this; zero lines of actual implementation exist in `app.js`).
- SQLCipher-encrypted SQLite vs. the old app's plaintext, 5–10MB-capped browser `localStorage`.
- AI API keys in OS keychain vs. the old app's plaintext `localStorage`.
- No plaintext password/email logging (the old app's `main.js` writes raw credentials to an unrotated `auth-debug.log` on every login attempt — a real security bug, not to be ported).
- A working audit trail (the old app's own `AuditService.log()` call at `app.js:148` references a method that doesn't exist and throws at runtime on AI-consent-accept).
- Slint accessibility roles/labels throughout (the old app has zero ARIA/accessibility attributes anywhere).

## 2. Feature Parity Matrix

| Feature | Old Project | Rust Project | Status | Priority |
|---|---|---|---|---|
| **Authentication** |
| Monthly HMAC password login | Yes — HMAC-SHA512(secret, email\|YYYY-MM), 3-attempt lockout (session-only) | Yes — identical algorithm, identical 3-attempt lockout | ✅ Match | — |
| Plaintext credential logging | Present (security bug) | Absent | ✅ Rust exceeds | — |
| **Client Management** |
| Create client | Yes | Yes | ✅ Match | — |
| Select/switch client | Yes (silent, no confirm) | Yes | ✅ Match | — |
| Edit client | **No** (QA checklist describes it; zero implementation in app.js) | Yes (real, wired to DB) | ✅ Rust exceeds | — |
| Delete client | **No** (same as above) | Yes (real, wired to DB) | ✅ Rust exceeds | — |
| **File Import** |
| Excel import (.xlsx/.xls/.xlsm) | Yes — 50-row header scan, 50+ column synonyms, 3000×36 limits | Yes — verified matching synonym/limit constants | ✅ Match | — |
| PDF text import | Yes — 5px row clustering, 60-row header scan, 40px merge gap, X-boundary fence-posts | Yes — identical constants verified in source + tests | ✅ Match | — |
| Scanned PDF / image OCR | Yes — Tesseract.js, eng/OSD1, 2.5× canvas scale | Yes — Tesseract shell-out (added earlier this session) | ✅ Match | — |
| Password-protected PDF retry | Yes | Yes | ✅ Match | — |
| CSV transaction import | **No** (not supported by either app) | No | ✅ Match (non-issue) | — |
| Bank-specific parser fixes (Cosmos FW, Kotak split-header, ICICI WM `(-)`/FD-cutoff, BOM/BOB type-buffering, SBI/ICICI split-date, IDBI noise-rows) | Yes, all 10+ documented | Yes — Cosmos parser confirmed present; gap/tolerance constants confirmed exact | ✅ Match | — |
| Batch folder processing (real path) | Yes — sequential, per-file dedup, single history entry | Yes | ✅ Match | — |
| Batch monitor (pause/resume/abort) | Built but **dead/unreachable from UI** | Not built | ✅ Match (neither works) | — |
| **Deduplication** |
| Cross-session hash dedup | Yes — Java-style hashCode-31 polynomial hash on date\|narration\|debit\|credit | Present, not independently re-derived hash-for-hash in this pass | ⚠️ Verify | Low |
| Intra-file exact dedup (incl. balance) | Yes | Present | ⚠️ Verify | Low |
| Reset dedupe history | Yes | Yes (confirmed wired) | ✅ Match | — |
| **Classification Engine** |
| Learned rules (client + global, hit count, 0.9/0.6 confidence) | Yes | Yes — identical confidence values, identical tier order | ✅ Match | — |
| Keyword heuristics (~24 categories) | Yes | Yes — 24 categories, explicit "port of `_kwMatch`" comment | ✅ Match | — |
| NEFT/RTGS/IMPS/UPI party-name extraction | Yes | Yes | ✅ Match | — |
| Inline GST/TAX tag detection | Yes | Yes | ✅ Match | — |
| Rich GST engine (12-category vendor map, rate/CGST/SGST/IGST, GSTIN extraction) | Yes (computed, never displayed) | Yes — **verified exact field-for-field match**, surfaced as `gst_rate`/`gst_amount`/`gst_type` on `Transaction` (improvement: old app never surfaced this, Rust persists it) | ✅ Rust matches and exceeds | — |
| Tally-group engine (19 groups, ~55 keyword entries) | Yes (`learnOverride` dead/unreachable) | Yes — same `KEYWORD_MAP` structure; not re-verified entry-by-entry this pass | ⚠️ Verify (low risk given GST engine's proven fidelity) | Low |
| AI classification (OpenAI/Claude/Gemini, consent-gated) | Yes — keys in plaintext `localStorage` | Yes — same 3 providers, same consent-gate pattern, keys in OS keychain | ✅ Rust matches and exceeds | — |
| Vendor canonicalization (`_normalizeVendors`) | Yes | Yes — `party_master.rs`, explicit port comment | ✅ Match | — |
| Confidence engine | Dead code (logic duplicated inline 6+ times in app.js) | Inline (matches actual old-app behavior, not its unused engine) | ✅ Match | — |
| **Editing & Manual Entry** |
| Edit transaction (Save & Learn / Save / Suspense / Delete) | Yes | Yes | ✅ Match | — |
| Manual transaction add | Yes | Yes | ✅ Match | — |
| Rules management (view/delete/backup/restore JSON) | Yes | Yes — backup/restore confirmed wired | ✅ Match | — |
| Ledger import from Excel | Yes (Excel **and** CSV via SheetJS) | Yes (Excel via `calamine`) — CSV path not confirmed | ⚠️ Possible minor gap | Low |
| Import history (view/load/remove) | Yes | Yes | ✅ Match | — |
| Re-import classified Excel | Yes | Yes — confirmed wired | ✅ Match | — |
| Auto-classify-all | Yes | Yes | ✅ Match | — |
| Audit trail + undo-last-edit | Yes (but `AuditService.log()` throws on AI-consent-accept — live bug) | Yes — real `audit_log` table, no crash | ✅ Rust matches and exceeds | — |
| **Export** |
| Export to Excel (4-sheet workbook) | Yes — direct download, no options (its options modal `modalExcelExport` is dead markup) | Yes — via a real, wired "excel-export" modal | ✅ Rust matches and exceeds | — |
| Dedicated Tally XML export (live preview, validation gating) | Yes | Yes — confirmed wired, same validation-blocks-export pattern | ✅ Match | — |
| Multi-software export wizard (Tally/Zoho/QuickBooks/Odoo confirmed; Excel-CSV/Generic-XML in old app) | 6 targets, 4-step wizard | 4 confirmed targets (Tally/Zoho/QuickBooks/Odoo); Excel-CSV/Generic-XML targets not confirmed reachable | ⚠️ Verify | Medium |
| Bank reconciliation (Tally voucher import, fuzzy date/amount match, CSV export) | Yes — greedy bipartite match, documented tolerances | Yes — `reconcile_parse_tally`/`reconcile_match`, configurable `recon_days`/`recon_pct` | ✅ Match | — |
| **Dashboard / Analytics** |
| Dashboard tab/view | Yes | Yes | ✅ Match | — |
| 8 summary cards | Yes | Yes — explicit "8 stat cards" comment in `dashboard.slint` | ✅ Match | — |
| 4 charts (monthly bar, expense doughnut, cash-flow line, top-vendors bar) | Yes | Yes — explicit "4 charts (2×2)" comment | ✅ Match | — |
| Insights strip (max debit/credit, averages, top vendor) | Yes | Yes | ✅ Match | — |
| Dashboard filters (date/bank/vendor/expense-head) | Yes | Present (`dash-filter-banks` etc. confirmed) | ✅ Match | — |
| Smart-filter drill-down (click a card/chart → filters main table) | Yes (`data-sf-*` system) | Yes — confirmed via `clickable: true` + `drill-filter()` pattern on dashboard cards | ✅ Match | — |
| **Settings** |
| Settings screen (narration/GST/reconciliation/logging, ~11 fields) | Yes (only ~11 of ~50+ `BSPConfig` fields are actually UI-exposed; rest hardcoded) | Yes — same ~11-field set | ✅ Match | — |
| **Data Layer / Persistence** |
| Persistence mechanism | Pure browser `localStorage`, 5–10MB ceiling, tied to one machine/profile | SQLCipher-encrypted SQLite, no practical size ceiling | ✅ Rust exceeds | — |
| Migration tool: old app's existing clients/rules/history → Rust DB | N/A | **Does not exist** | ❌ Gap | High (for production cutover only) |
| **Transaction Table UI** |
| Column set (14 columns: Bank Name, Account No, Date, Narration, Ref, Debit, Credit, Balance, Vendor/Customer, Ledger for Posting, Expense Head, Type/Status, Tags, Review) | Yes | Yes — verified identical column list, order, and labels | ✅ Match | — |
| Filter buttons (All/Unreviewed/Suspense/High Conf./Duplicates/GST-Tax/Needs Review) + bank dropdown + compact toggle | Yes, multi-select toggle semantics | Yes — verified identical 7 buttons + bank filter + compact toggle | ✅ Match | — |
| Active filter chip bar (removable chips) | Yes | Yes — verified | ✅ Match | — |
| Global date-range bar + quick-range buttons (Today/This Month/Last Month/Current FY/Prev FY) | Yes, 5 buttons | 4 confirmed (This Month/Last Month/Current FY/Prev FY); "Today" not confirmed | ⚠️ Verify | Low |
| Summary panel (13 sub-sections incl. reconciliation banner, classification quality, recurring parties, receipts/payments by ledger) | Yes, heavily smart-filter-linked | Not exhaustively re-verified section-by-section this pass | ⚠️ Verify | Low |
| Accessibility (`accessible-role`/`accessible-label`) | **None anywhere** | Present (added earlier this session) | ✅ Rust exceeds | — |

## 3. Gap Classification

### Critical
*(None found.)* No user-visible workflow, screen, or business-logic computation that the old app actually exercises in production is missing from the Rust app.

### High
1. **No migration path for existing old-app data.** If any real client/rules/import-history/dedupe data exists in the Electron app's `localStorage` for a production user, there is no tool — in either direction — to carry it into the Rust app's encrypted SQLite DB. The old app's existing "Backup Rules" JSON export covers rules+ledgers only (not clients, import history, or dedupe hashes), and Rust's restore-rules importer has not been confirmed schema-compatible with that JSON format. This is the one finding that materially blocks an honest "100% replacement" claim for any user who already has data in the old app — it is a one-time migration-tooling task, not a missing feature in the new app's day-to-day operation.

### Medium
2. **Export-wizard software coverage not fully confirmed.** Old app's wizard offers 6 targets (Tally, Zoho, QuickBooks, Odoo, Excel/CSV, Generic XML); Rust confirmed has Tally/Zoho/QuickBooks/Odoo wired to real generators. Whether the remaining two targets are reachable through Rust's wizard (vs. already covered by Rust's separate, already-confirmed "Export to Excel" and dedicated "Tally XML" buttons) needs a direct check before calling this fully closed.

### Low
3. Ledger import: old app accepts `.csv` in addition to Excel (via SheetJS, which parses both); Rust's `calamine`-based importer may be Excel-only. Worth a one-line check; trivial to add if missing.
4. "Today" quick-date-range button not confirmed present in Rust (4 of the old app's 5 quick-range buttons were directly verified).
5. Tally-group engine's ~55 keyword-to-ledger-group mappings were structurally confirmed (same `KEYWORD_MAP` pattern) but not individually diffed entry-by-entry, unlike the GST engine which was fully verified. Given the GST engine's proven 100% fidelity, risk here is low, but it's the one remaining engine not constant-for-constant confirmed.
6. Old app's intra-file/cross-session dedup hash algorithms (Java-style hashCode-31 polynomial hash) were not independently re-derived against Rust's implementation this pass.
7. Old app's 13-subsection Summary panel (reconciliation banner, classification-quality bar, recurring-parties list, receipts/payments-by-ledger breakdowns) was not verified section-by-section against Rust's summary panel.

## 4. Roadmap to Close Remaining Gaps

### Phase 1 — Verification pass (no code changes expected; closes most ⚠️ items above)
- Diff Rust's `tally_group_engine.rs` `KEYWORD_MAP` against the old app's 55-entry list (same method used for `gst_engine.rs` in this audit).
- Confirm Rust's dedup hash function produces identical hex output to the old app's hashCode-31 algorithm for a shared test vector.
- Confirm/add a "Today" quick-range button if missing.
- Confirm `calamine`-based ledger import accepts `.csv`; add a CSV branch if not (small, isolated change).
- Walk the Summary panel's 13 sub-sections side-by-side in both apps with a loaded sample statement; list any missing sub-section.
- Confirm whether Rust's export wizard reaches Excel/CSV and Generic-XML generation, or whether those are intentionally redundant with the separately-confirmed Excel/Tally export buttons.

**Estimated effort:** 1–2 days. Expected outcome: most Medium/Low items reclassify to ✅ Match; at most 1–2 small, isolated fixes.

### Phase 2 — Migration tooling (closes the one High gap)
- Build a one-time importer that reads the old Electron app's `localStorage` (via Electron's on-disk LevelDB-backed storage files, or by adding a one-time "Export everything" button to the old app that the Rust app can then import) and populates: clients, rules, ledgers, dedupe hashes, and — ideally — reconstructible import history (raw transactions per import).
- Decide and document a cutover procedure for any real existing users: run the migration once, verify counts match, then retire the old app.

**Estimated effort:** 2–4 days, depending on whether `localStorage` is read directly (more fragile, no old-app code changes) or via an old-app-side export feature (more robust, requires touching the old app once more).

### Phase 3 — Sign-off
- Re-run this matrix after Phases 1–2; expect zero remaining ❌ rows.
- Manual end-to-end smoke test against the old app's own real sample statements already present in `assets/` (12 real bank-statement files across 11 banks) — these are real production-format fixtures that neither app's automated test suite currently uses, and are valuable regression material for both.

## 5. Parity Estimate

| Metric | Estimate |
|---|---|
| **Functional parity (workflows a user can actually complete)** | ~97% |
| **UI parity (screens/fields/filters verified matching)** | ~95% — pending the Phase-1 verification items above |
| **Export parity** | ~90% — pending confirmation of the 2 remaining export-wizard targets |
| **Data-migration readiness for existing users** | 0% — no tooling exists yet (Phase 2) |
| **Overall release readiness for a brand-new user with no prior old-app data** | **Ready**, pending the Phase 1 verification pass |
| **Overall release readiness as a 100% drop-in replacement for an existing old-app user** | **Not ready** until Phase 2 (migration tooling) lands |

**Bottom line:** the Rust application is not "catching up" to the Electron app in the way a typical rewrite-parity audit usually finds — the business logic, classification engines, export pipeline, dashboard, and most UI surfaces are already faithful, verified ports, in several cases exact field-for-field matches. The one real blocker to an unqualified "100% replacement" claim is the absence of a path to migrate an existing user's accumulated data out of the old app's browser `localStorage` and into the new app's database. Everything else identified above is either already at parity or a small, low-risk verification/fix task.
