# Parity Gap Report — bank-statement-processor-rust vs bank-statement-processing (Electron)

Generated: 2026-06-13

---

## Fully Matched (35 features)

| Feature | Electron | Rust |
|---|---|---|
| Login + HMAC auth | `main.js validate_credentials()` | `src/main.rs on_do_login`, `src/auth.rs` |
| Client selection | `app.js _selectClient()` | `src/main.rs on_do_select_client` |
| New Client modal | `app.js _saveClient()` | `src/main.rs on_do_new_client` |
| Edit Client modal | — (Rust-only) | `src/main.rs on_do_edit_client` |
| Delete Client | — (Rust-only) | `src/main.rs on_do_delete_client` |
| Load single file XLSX/PDF | `app.js _loadFile()` | `src/main.rs on_do_load_file` |
| PDF password unlock | `app.js Parser._confirmPassword()` | `src/main.rs on_do_pdf_pwd_confirm` |
| OCR scanned PDF | Tesseract.js | `src/parser/ocr_extractor.rs` |
| Batch Process Folder | `app.js _loadBatch()` | `src/main.rs on_do_batch_folder` |
| Reset Dedupe History | `app.js DB.resetDedupeHashes()` | `src/main.rs on_do_reset_dedupe` |
| Auto-Classify All | `app.js _classify()` | `src/main.rs on_do_auto_classify` |
| Transaction table 14 cols | `index.html #txnTable` | `ui/main_screen.slint TxnTableRow` |
| Row color coding | `app.js row-color` | `src/main.rs row_color` |
| Status filter buttons | `.fbtn` buttons | `src/main.rs compute_filter_counts` |
| Bank filter dropdown | `app.js bankFilterEl` | `src/main.rs on_do_bank_filter` |
| Compact mode toggle | `app.js btnCompactMode` | `ui/main_screen.slint compact` |
| Global date range filter | `app.js _applyDateRange()` | `src/main.rs on_do_date_filter_apply` |
| Date quick-preset buttons | `app.js _setQuickRange()` | `src/main.rs on_do_date_preset` |
| Edit Transaction modal | `app.js _saveTxn()` | `src/main.rs on_do_save_txn` |
| Save & Learn | `app.js _saveTxn(true)` | `src/main.rs on_do_save_txn` learn=true |
| Delete Transaction | `app.js _deleteTxn()` | `src/main.rs on_do_delete_txn` |
| Mark as Suspense | `app.js _markSuspense()` | `src/main.rs on_do_mark_suspense` |
| Add Transaction Manually | `app.js _saveAddTxn()` | `src/main.rs on_do_add_txn` |
| View Rules modal | `app.js _openRules()` | `src/main.rs on_do_view_rules` |
| Import History + reload | `app.js _openHistory()` | `src/main.rs on_do_import_history` |
| Dashboard 4 charts | `dashboard.js` | `src/analytics.rs push_dashboard` |
| Dashboard summary cards | `index.html #dashCardsRow` | `src/main.rs push_dashboard` |
| Dashboard insights strip | `index.html #dashInsightsRow` | `src/main.rs push_dashboard` |
| Dashboard filters | `.dash-filters` | `src/main.rs on_do_dash_filter` |
| Export to Excel XLSX/CSV | `app.js _exportExcel()` | `src/export/excel.rs` |
| Quick Tally XML Export | `app.js _exportTallyRun()` | `src/export/tally.rs` |
| Reconciliation | `src/engines/reconciliation.js` | `src/main.rs on_do_reconcile` |
| Export Reconciliation CSV | `app.js _exportReconciliation()` | `src/main.rs on_do_export_recon_csv` |
| Audit Trail modal | `app.js _openAuditTrail()` | `src/main.rs on_do_audit_trail` |
| Download Audit Logs | `app.js BSPLogger.downloadLogs()` | `src/main.rs on_do_download_logs` |

---

## Partially Matched (11 features)

| Feature | Electron | Rust | Gap |
|---|---|---|---|
| AI Classify | `src/engines/ai-classifier-engine.js` | `src/ai_classifier.rs` | No AI Consent modal; no scope selector; no user feedback on AI failure |
| AI Settings | `index.html #modalAISettings` | `ui/app.slint ai-*` props | Missing classify scope (unclassified-only vs all) |
| Rules Backup | `app.js _backupRules()` | `ui/main_screen.slint` MBtn | Button rendered but no callback wired |
| Rules Restore | `app.js _restoreRules()` | `ui/main_screen.slint` MBtn | Button rendered but no callback wired |
| Import Ledgers | `app.js _loadLedgerFile()` | stub_callback | Explicit stub; button disabled |
| Re-import Excel | `app.js _reimportClassified()` | stub_callback | Explicit stub; button disabled |
| Settings save | `app.js _saveConfig()` 11 fields | `src/main.rs on_do_settings_save` | Only saves AI provider/key/enabled; 8 other sections ignored |
| Batch Monitor | `src/workers/batch-processor.js` | static summary only | No Start/Pause/Abort/progress bar |
| Audit Trail Undo | `app.js _undoLastEdit()` | button always disabled | No undo stack |
| Tally Export preview | live XML update | static placeholder | No dynamic preview |
| Reconciliation tolerances | reads cfgReconDays/cfgReconAmt | hardcoded ±7 days, ±0.01 | Settings not wired to reconcile logic |

---

## Missing Features (14 features)

| Feature | Electron | Reason |
|---|---|---|
| Image import PNG/JPG/TIFF/BMP | `index.html accept=".png,.jpg…"` | File picker limited to pdf/xlsx only |
| Deduplication toggle checkbox | `index.html #chkDedupe` | No UI toggle; dedup always runs |
| Multi-dimension filter chip bar | `app.js _applyFiltersMulti()` | Single-dimension active_filter only |
| Dashboard card drill-down | `app.js applySmartFilter()` | Cards display-only, no click→filter |
| Narration Cleaner engine | `src/services/narration-cleaner.js` | Not ported to Rust |
| Tally Group Engine | `src/engines/tally-group-engine.js` | Not ported to Rust |
| Transaction validation pipeline | `app.js _validateTransaction()` | No structural validation (zero amounts, missing dates) |
| Auto-seed ledgers | `app.js _autoSeedLedgers()` | No equivalent |
| Confidence recalculation engine | `src/engines/confidence-engine.js` | Single scalar set at classify time only |
| Export Wizard full options (steps 2-4) | checkboxes: OB, GST, Ledger, Narrations, Classified only, Skip low-conf | All checkboxes ignored, hardcoded defaults |
| Settings — Clear All Logs | `app.js BSPLogger.clearLogs()` | TouchArea has no clicked handler |
| Multi-account OB/CB tracking | `app.js state.accounts[]` | Single opening_balance in AppState |
| Active filter chip individual removal | `app.js _toggleFilter()` | Chips rendered but no remove callback |
| AI Consent modal | `index.html #modalAIConsent` | Entirely absent from Rust UI |

---

## Backend Mismatches

| Area | Electron | Rust | Notes |
|---|---|---|---|
| Persistence | localStorage (volatile) | SQLite (durable) | Rust is better |
| OCR runtime | Tesseract.js in-browser | tesseract CLI on PATH | Rust fails silently if tesseract absent |
| Narration normalization | NarrationCleaner engine | Raw narration stored | Rust shows raw bank strings |
| Tally group assignment | TallyGroupEngine assigns partyGroup | account_head only | No Tally group hierarchy |
| Validation pipeline | routes to needs_review on zero/missing | only classifier sets needs_review | Structural issues not caught |
| Confidence recalculation | multi-signal | single scalar | Rule strength/review age signals absent |
| Reconciliation tolerances | configurable from Settings | hardcoded | Settings not wired |

---

## UI Mismatches

| UI Element | Electron | Rust |
|---|---|---|
| Filter chip bar | functional with × removal, Clear All, multi-dim | renders but no remove/Clear All callback |
| Settings modal | 11 live fields, all persisted | 3 fields wired, others display-only |
| Dedup toggle | toolbar checkbox | absent |
| Batch Monitor progress | progress bar + per-file table | static text summary |
| Dashboard cards | clickable → filter transactions | display only |
| Tally export preview | live XML | placeholder text |
| AI Consent modal | full modal with legal text + checkbox | absent |

---

*Implementation order: Narration Cleaner → Tally Group Engine → Settings persistence → Clear Logs → Active filter chip bar → Rules Backup/Restore → Dedup toggle → Dashboard drill-down → AI Consent modal → Image import → Validation Pipeline → Multi-filter → Export Wizard options → Reconciliation settings*
