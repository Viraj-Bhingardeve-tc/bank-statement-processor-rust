# Bank Statement Processor (Rust/Slint) — Full Project Audit

**Date:** 2026-07-06 · **Audited HEAD:** `d0dc9fc` · **Auditor:** Lead-architect pass, read-only — no code was modified to produce this report.
**Method:** Direct inspection of all `.rs`/`.slint` source, `Cargo.toml`, git history (55 commits, since 2026-06-03), `cargo test` execution, and a fresh line-by-line diff against the sibling Electron app (`bank-statement-processing`). Three prior in-repo audits were read for baseline (`docs/PRODUCTION_READINESS_AUDIT_2026-06-22.md`, `PARITY_AUDIT_2026-06-24.md`, `PARITY_GAP_REPORT.md`) — the first two are high quality but two weeks stale; **`PARITY_GAP_REPORT.md` is confirmed unreliable by the codebase's own history and is ignored here.** Every claim below was re-verified against current code, not against those documents' claims.

**Headline:** this is a genuinely capable rewrite — parsing, classification, GST/Tally logic, security, and UI are much further along than a typical "rewrite in progress" would be at 5 weeks old. The gap to shippable is now concentrated in a small number of specific things: dead Settings fields, one un-scoped dedup discrepancy, zero CSV transaction import, no migration tooling for existing old-app users, no CI/packaging/docs, and a 2,777-line `main()`. Nothing found is a correctness catastrophe.

---

# 1. Current Completion

| Area | Status | % | Notes |
|---|---|---|---|
| Core Engine (orchestration) | 🟡 | 80% | Works end-to-end; `main()` is one 2,777-line function (`src/main.rs:1464-4241`) — architecturally weak, not functionally broken |
| Parser (PDF text) | ✅ | 95% | Multi-stage fallback, verified constants match old app exactly |
| Parser (Excel) | ✅ | 90% | Matches old app's 50-row scan / synonym list; bank-detection not called from Excel path (blank `bank_name` on Excel-only imports) — long-standing, unfixed |
| OCR | 🟡 | 80% | Now backgrounded off UI thread with a real progress bar (fixed since last audit). Still skips `validate_balances`/`deduplicate_txns`, and its 2% tolerance floor lacks the `.max(1.0)` guard present in the PDF/Excel paths — a real, unfixed divergence |
| PDF Import | ✅ | 95% | Password-protected PDFs supported, matches old app |
| Image Import | ✅ | 90% | PNG/JPG/TIFF/BMP via Tesseract shell-out; fragile external dependency (fails silently if Tesseract not on PATH) |
| Excel Import | ✅ | 90% | See Parser (Excel) above |
| CSV Import (bank statements) | ❌ | 0% | Not accepted by either app — non-issue for parity, but still a real limitation vs. modern expectations. File filter `main.rs:1614` has no `.csv` |
| Transaction Extraction | ✅ | 95% | Format-agnostic column detection, handles merged cells, multi-line narrations |
| Duplicate Detection | 🟡 | 80% | Cross-session hash now verified JS-parity-equivalent (fixed this cycle). **New finding: intra-file dedup excludes balance in Rust but includes it in the old app** — a real, previously-undocumented discrepancy (see §2) |
| Narration Cleaning | 🟡 | 70% | Ported and running, but `payment_ref`/`txn_type` outputs are computed then discarded (`main.rs:940-970`) — same gap as before, unfixed |
| GST Analysis | 🟡 | 80% | Now wired into `Transaction` and into Zoho/generic-XML exports (fixed this cycle). `gstins`, `suggested_ledger`, `confidence` fields still computed and discarded |
| Analytics | ✅ | 90% | Dashboard aggregates correct; zero dedicated unit tests |
| Dashboard | ✅ | 90% | 8 stat cards, 4 charts, 3-of-8-card drill-down, Suspense-conditional row (recovered post-crash in `d0dc9fc`) |
| Search | 🟡 | n/a | No dedicated search box found; filtering serves this role — see UI audit |
| Filters | ✅ | 90% | Status chips + bank/vendor/head filters, individual chip removal, Clear All all wired. Minor: Clear-All control lacks an accessibility label |
| Export (Excel) | ✅ | 90% | 4-sheet workbook via a real wired modal |
| Export (Tally XML) | ✅ | 85% | Live-generated, validation-gated |
| Export Wizard (6 targets) | ✅ | 90% | **All 6 targets now confirmed wired to real generators** (Tally/Zoho/QuickBooks/Odoo/Generic-CSV/Generic-XML) — closed since last audit |
| Tally XML | ✅ | 85% | See above |
| Database | 🟡 | 80% | SQLCipher encryption + versioned migrations now in place (major fixes this cycle). `classification_rules` still has no `UNIQUE` constraint — dup-rule bug unfixed |
| Rules Engine | 🟡 | 75% | Rule CRUD, Backup/Restore now wired (closed since last audit). Duplicate-rule accumulation bug remains |
| Vendor Detection | ✅ | 90% | `party_master.rs`, explicit port of `_normalizeVendors` |
| UI (screens/modals) | ✅ | 85% | 16 modal states + 3 overlays, no stub callbacks found anywhere. See full UI audit in §3 |
| Settings | 🟡 | 45% | Only 2 of ~11 exposed fields (`recon_days`, `recon_pct`) actually affect behavior; 6 more persist but are never read by business logic; 1 (Default State dropdown) doesn't even persist |
| Logging | 🟡 | 60% | `log`/`env_logger` present; "Clear All Logs" wiring not independently re-confirmed this pass; download-logs previously confirmed working |
| Error Handling | 🟡 | 75% | DB-write-failure swallowing fixed (major cycle fix); still no domain-specific error types (`thiserror` remains an unused dependency); toast has no auto-dismiss timer |
| Testing | 🟡 | 55% | 365/365 unit tests pass under `--lib`; the bin-crate test build still fails to compile (17 errors) so **no single command runs the full suite**; zero integration tests; zero real-file fixtures; `main.rs`/`ai_classifier.rs`/`export/tally.rs`/`export/excel.rs`/`analytics.rs` remain fully untested |
| Packaging | ❌ | 10% | No installer, no auto-updater, no release pipeline, version pinned at `0.1.0`, SQLCipher DLL copy-on-build only (recent fix) |
| Documentation | ❌ | 15% | No README, no user manual, no CONTRIBUTING/architecture docs; only audit reports exist in-repo |

**Overall estimated completion: ~72%** — core domain logic and security are ahead of typical rewrite-stage expectations; packaging, documentation, full test-suite runnability, and a handful of specific wiring gaps (Settings, dedup, GST residual fields) are what's holding the number down.

---

# 2. Comparison Against the Old Electron App

The 2026-06-24 in-repo parity audit already closed most gaps to ~97% functional parity. This pass re-verified its 7 open items directly against current source (not trusting either audit's prior claims):

| # | Item | Status now |
|---|---|---|
| 1 | Tally-group `KEYWORD_MAP` (55 entries, 21 groups) | ✅ Closed — exact match confirmed, spot-checked 12 entries |
| 2 | Dedup hash algorithm | 🟡 Half-closed — cross-session hash is now JS-equivalent, but **new finding**: intra-file exact-dedup includes `balance` in the old app's key but Rust's `detect_duplicates` reuses the balance-excluding hash — a real behavioral divergence (two rows with same date/narration/amounts but different balance: old app treats as distinct, Rust flags as duplicate) |
| 3 | "Today" quick-range button | ✅ Closed — present in both |
| 4 | Ledger import CSV support | ✅ Closed — Rust now has a dedicated CSV branch |
| 5 | 13-subsection Summary panel | ✅ Closed — all 13 present, only cosmetic layout differences |
| 6 | Export wizard 6 targets | ✅ Closed — all 6 wired to real generators |
| 7 | Migration tooling (old-app → new-app data import) | ❌ Still open — zero references to `localStorage`/migration-of-old-data anywhere in the Rust repo |

### Grouped by priority

**Critical**
- *(None.)* No user-visible workflow the old app actually exercises in production is missing from Rust.

**Important**
1. **No migration path for existing old-app users' data** (clients, rules, import history, dedupe hashes trapped in the Electron app's `localStorage`). Blocks an honest "100% drop-in replacement" claim for anyone with real prior usage. Not needed for brand-new users.
2. **Intra-file dedup balance discrepancy** (new finding, §2 item 2) — a real behavioral difference that could cause false-positive duplicate flags on legitimately distinct transactions.
3. **Excel-path bank-detection not invoked** — Excel-only imports get a blank `bank_name` where the old app would classify it (long-standing, carried over from the June 22 audit, still unfixed).

**Nice to Have**
4. OCR path doesn't run `validate_balances`/`deduplicate_txns` and has a missing `.max(1.0)` tolerance floor (inconsistent with PDF/Excel paths, low real-world frequency since OCR is already the fallback path).
5. `payment_ref`/`txn_type` from narration cleaning computed but not surfaced anywhere (parity with old app's own dead code here, so low urgency).
6. `gstins`/`suggested_ledger`/`confidence` from GST analysis still discarded past the tag/rate/amount fields already wired.

---

# 3. UI Audit

16 `modal-state` values + 3 standalone overlays, all in `ui/main_screen.slint` with callbacks in `src/main.rs`. **No `stub_callback` exists anywhere** — every `on_do_*` has real logic.

| Screen/Modal | Status | Notes |
|---|---|---|
| Login | ✅ Complete | — |
| Main transaction screen | ✅ Complete | 13 fixed-width columns (~1196px total); only Narration stretches — **still not responsive** to window/DPI changes |
| Dashboard | ✅ Complete | 8 stat cards (3 clickable drill-down), 4 charts in 2×2 |
| New/Edit/Delete Client | ✅ Complete | — |
| Add/Edit Transaction | ✅ Complete | Save / Save & Learn / Suspense / Delete all wired |
| Confirm dialog (new since last audit) | ✅ Complete | Generic Yes/Cancel replacing native `confirm()`, 6 actions routed through it — genuine step toward a shared modal shell |
| View Rules / Backup / Restore | ✅ Complete | Backup/Restore callbacks now real (previously stubbed — fixed) |
| **Import Ledgers** | 🟡 Partial | Backend fully implemented (`main.rs:2179-2260`, Excel+CSV) but the toolbar **button is `enabled: false`** — feature is unreachable by any user despite working code underneath |
| Re-import Excel | ✅ Complete | — |
| Batch Monitor | 🟡 Partial | Real per-file status table (bank/account/period/count/status) — no longer a static summary. But no aggregate progress bar, and Pause/Abort remain permanently disabled; batch runs synchronously and can't be interrupted |
| OCR progress overlay | ✅ Complete (fixed) | Backgrounded thread, real progress bar, was previously fully dead |
| Reconcile modal | ✅ Complete | Full match pipeline works; the **"Run Reconciliation" button itself is still permanently disabled** — functionality is reachable only via the adjacent "Import Tally Export" button, a confusing but non-blocking UX wart |
| Import History | ✅ Complete | — |
| Audit Trail | ✅ Complete | Single-level undo |
| AI Consent / AI Settings | ✅ Complete | Real, mandatory consent gate; disclosure copy slightly over-states what's transmitted (narration only, not amounts/vendor) |
| Excel Export / Tally Export / Export Wizard | ✅ Complete | All 6 wizard targets wired |
| **Settings screen** | 🟡 Partial (functionally the weakest screen) | 9 controls total. Only `recon-days`/`recon-pct` actually affect behavior. `narr-*`, `gst-*`, `log-level` persist to DB and redisplay but are never read by any business logic — decorative. The "Default State" combo box isn't even wired to persistence. This is the single biggest "looks done, isn't" surface in the UI |
| Filter chip bar | ✅ Complete | Individual removal + Clear All both work; Clear-All control lacks an accessibility label (minor) |
| Accessibility | 🟡 Partial, improved | ~50 `accessible-role/label` occurrences vs. ~229 interactive controls — roughly 1-in-5 labeled, up from zero. Esc-to-close now works via a `FocusScope`. No other keyboard shortcuts (Ctrl+S/Ctrl+Z/Ctrl+F) exist |

**Net UI verdict:** visually and functionally complete for the golden path; the two "button exists but does nothing/disabled" cases (Import Ledgers, Run Reconciliation) and the mostly-decorative Settings screen are the concrete items a QA pass would flag first.

---

# 4. Missing Business Logic

1. **Settings not actually driving behavior** — narration-cleaning toggles, GST toggles, and log-level are stored but never consulted by `narration_cleaner.rs`, `gst_engine.rs`, or the logging setup.
2. **Intra-file dedup balance exclusion** — diverges from old app's balance-inclusive key (§2).
3. **Excel-path bank detection never invoked** — `excel_parser.rs` doesn't call `bank_detection::detect()`.
4. **OCR path skips validation/dedup passes** that PDF/Excel run.
5. **GST `gstins`/`suggested_ledger`/`confidence`** computed, never surfaced past rate/amount/type.
6. **Narration `payment_ref`/`txn_type`** computed, never surfaced.
7. **No whole-statement opening+credits-debits=closing reconciliation** — only per-row running-balance checks exist, and those are advisory-only (never block import).
8. **`classification_rules` duplicate-rule bug** — no `UNIQUE` constraint, `INSERT OR IGNORE` is a silent no-op, duplicates can accumulate unnoticed.
9. **No real multi-user/RBAC** — the monthly-password gate is explicitly documented (as of this cycle) as licensing-only, not access control; there's still exactly one shared credential for all users of an installation.

---

# 5. Missing Database Work

- No `UNIQUE` constraint on `classification_rules` (dup-rule accumulation).
- `import_history.file_hash` column exists but is never queried (vestigial).
- No bulk-write batching beyond `upsert_transactions`/`add_dedupe_hashes` (now transactional — good); other multi-row write paths not confirmed to share this treatment.
- Migration framework itself is now solid (versioned, tested, hard-fails on error) — this section's remaining items are narrower than before.
- No migration tooling to import the old Electron app's `localStorage` data (clients/rules/history/dedupe hashes) into this schema — the one substantive open item from the parity audit.

---

# 6. Missing Export Features

- Tally XML and generic-XML exports don't include a GST/tax breakup line-by-line (only aggregate rate/amount/type per transaction, not a CGST/SGST/IGST column split).
- No running-balance column in any export format.
- No formal printable financial statements (P&L, ledger-format reports) — only raw transactional export and dashboard-level BI reporting.
- `TallyOpts.gstin/fy` remain intentionally unread by the TDML generator (documented as out-of-scope, matches the old app's own Tally exporter — not a bug, a scoping decision worth confirming with a real Tally user before calling it closed).

---

# 7. Missing OCR Improvements

- Still fragile-by-design: shells out to a system-installed Tesseract binary; no bundled OCR library, silent degrade if Tesseract is absent from PATH.
- Skips `validate_balances`/`deduplicate_txns` that the PDF/Excel paths run.
- Missing the `.max(1.0)` tolerance floor present elsewhere — a real, minor correctness divergence on very small-amount statements.
- No OCR accuracy tuning/config exposed (fixed 2.5× canvas-scale-equivalent behavior, no user-adjustable DPI/language).

---

# 8. Missing Parser Improvements

- No CSV bank-statement transaction import (neither app supports this — a parity non-issue, but a real gap vs. modern user expectation; regional/smaller banks increasingly export CSV).
- Excel path doesn't invoke bank auto-detection (item 3 above).
- Dedup hash is 32-bit and collision-theoretical-risk isn't load-tested; balance-exclusion divergence noted in §2.
- No password-protected Excel support (PDF has it, Excel doesn't).
- Bank coverage capped at the ~45 IFSC/pattern set ported from the old app — no mechanism to add new banks without a code change.

---

# 9. Missing Tests

- **No single command runs the entire test suite** — `cargo test --bin ... --no-default-features` still fails with 17 compile errors; `src/lib.rs` still excludes `auth`/`ui`/`settings`/`ai_classifier`, so those modules' tests never run under the passing `--lib` command.
- Zero tests in: `main.rs` (2,777-line orchestrator), `ai_classifier.rs`, `export/tally.rs`, `export/excel.rs`, `analytics.rs`.
- No integration tests, no real-file fixtures (no `tests/` directory, no sample PDF/Excel/image files checked in — even though the old app's own `assets/` folder has 12 real bank-statement PDFs/XLS across 11 banks that would make excellent shared regression fixtures for both apps).
- No end-to-end pipeline test (import → dedup → classify → export) verifying final file content.
- No test asserting exported GST figures are numerically correct against known input amounts.
- No fuzz/property-based testing on parsers.
- No load/perf test at any meaningful transaction-volume scale.

---

# 10. Production Readiness

**What's already resolved since the last formal readiness audit (2026-06-22):**
- Data-at-rest encryption (SQLCipher) — done.
- Versioned, hard-failing DB migration framework — done.
- DB write-failure swallowing — done, all sites now handled.
- Bulk-insert transaction wrapping — done.
- AI API key moved out of plaintext storage into OS keyring — done.
- GST engine output wired into `Transaction` + 2 of 3 export formats — done.
- OCR moved off the UI thread with a real progress overlay — done.
- Accessibility roles added (partial coverage) — started.
- Export wizard closed to all 6 targets — done.

**What still blocks a production release today:**
1. **Settings screen is mostly decorative** — a user configuring narration/GST/logging behavior will see zero effect; this is a trust-eroding "looks configurable, isn't" surface for any real deployment.
2. **No migration tooling** for existing old-app users — blocks cutover for anyone with real prior data, not brand-new users.
3. **No CI, no packaging/installer, no README/user documentation** — this repo cannot currently be handed to an end user or a second developer without hand-holding. Version is still `0.1.0`.
4. **Test suite doesn't run as one command** and has zero coverage on the entire UI-orchestration layer (`main.rs`) and all three export generators — correctness of the actual shipped binary, not just its internals, is unverified by automation.
5. **Auth is licensing-only by design** (now explicitly documented as such) — acceptable if this is genuinely a single-tenant desktop tool with no real access-control requirement, but this must be a conscious, communicated decision to whoever deploys it, not a silent assumption.
6. **Dup-rule bug and intra-file dedup discrepancy** are real, if narrow, correctness risks that would surface as confusing behavior to an end user over time (rules silently duplicating; occasional false-positive duplicate flags).
7. **`main()` at 2,777 lines** — not a functional blocker, but the single biggest risk to safely shipping *any* further change without regression, given the near-total absence of tests over that surface.

**Bottom line:** for a single new user with no prior old-app data, on a single machine, accepting "licensing-only auth," this is close to shippable on functionality — the honest blockers are operational (no CI/packaging/docs/full-test-run) and the Settings-screen credibility gap, not missing core capability.

---

# 11. Priority Roadmap

### Phase 1 — Critical (before any real user touches this)
1. Wire the 6 dead Settings fields (`narr-*`, `gst-*`, `log-level`) into actual behavior, or remove them from the UI if truly out of scope — currently misleading. **(2-3 days)**
2. Fix the intra-file dedup balance-exclusion discrepancy vs. the old app. **(0.5-1 day)**
3. Add a `UNIQUE` constraint (or pre-check) so `classification_rules` stops silently accumulating duplicates. **(0.5 day)**
4. Fix the test-build gap so `auth`/`ui`/`settings`/`ai_classifier` tests actually run, and get one command to run the full suite (fold bin-only helpers behind `#[cfg(test)]`-safe shims or move them into the lib crate). **(1-2 days)**
5. Enable the Import Ledgers button (backend already works) or explicitly remove/hide it if intentionally descoped. **(0.5 day)**
6. Fix or remove the permanently-disabled "Run Reconciliation" button. **(0.5 day)**

**Phase 1 estimate: ~5-8 days.**

### Phase 2 — Important (before calling this a real product)
7. Build old-app-to-new-app migration tooling (clients/rules/history/dedupe hashes out of Electron `localStorage` into the SQLCipher DB). **(3-5 days, per the parity audit's own estimate)**
8. Wire bank-detection into the Excel import path. **(1 day)**
9. Bring OCR path to parity with PDF/Excel (`validate_balances`/`deduplicate_txns` calls, `.max(1.0)` tolerance floor). **(1-2 days)**
10. Surface remaining GST fields (`gstins`, `suggested_ledger`, `confidence`) and narration `payment_ref`/`txn_type` where useful, or formally decide they're intentionally internal-only. **(2-3 days)**
11. Add integration tests against real file fixtures (reuse the old app's 12 real bank-statement samples) covering import → dedup → classify → export end-to-end. **(3-5 days)**
12. Add a README, a minimal user manual, and basic CI (build + test on push). **(2-3 days)**

**Phase 2 estimate: ~12-19 days.**

### Phase 3 — Polish
13. Break up `main()` into feature-scoped modules. **(3-5 days)**
14. Extend accessibility labeling beyond ~20% coverage; add keyboard shortcuts. **(2-3 days)**
15. Make the transaction table responsive instead of fixed-pixel. **(2-3 days)**
16. Add batch-monitor progress bar + working Pause/Abort. **(2-3 days)**
17. Packaging: installer, auto-update story, version bump past `0.1.0`. **(3-5 days)**
18. GST/tax breakup columns in exports; whole-statement balance reconciliation check. **(3-5 days)**

**Phase 3 estimate: ~15-24 days.**

### Total estimated remaining effort: **~32-51 working days** (roughly 6.5-10 weeks for a single developer at the pace already demonstrated — 55 commits in 5 weeks) to go from current state to a genuinely production-ready, documented, migratable, fully-tested release. The core domain logic does **not** need this time — it's already strong. The time is almost entirely in Settings wiring, test-suite completeness, migration tooling, and operational maturity (CI/packaging/docs).
