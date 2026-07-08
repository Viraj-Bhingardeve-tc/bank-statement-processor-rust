# Same-Client Opening-Balance Collision Fix — Implementation Report

**Date:** 2026-07-08
**Design approved in:** `SAME_CLIENT_OPENING_BALANCE_COLLISION_DESIGN.md` (Option B)
**Stability proof:** verified in the prior turn (hash inputs are pure functions of file content — no wall-clock, row-index, or hashmap-ordering dependency in any of the three pipelines).

---

## Change

`src/parser/excel_parser.rs`, `prepend_opening_balance_row` — the opening-balance row's `id` is no longer the hardcoded literal `"opening_balance"`. It's now derived as:

```rust
let ob_id = format!("opening_balance_{}", first.hash());
```

using `first` (`&txns[0]`), already in scope for the existing opening-balance-value derivation immediately above. One function changed; shared unconditionally by all three import pipelines (Excel, PDF, OCR all call it), so the fix applies to all of them without further changes.

No other production code was modified.

## Requirements checklist

1. **Changed only inside `prepend_opening_balance_row`** — confirmed; no other function touched.
2. **Literal replaced with `opening_balance_{txns[0].hash()}`** — done, using the in-scope `first` reference.
3. **Preserved existing behavior:**
   - `if txns.is_empty() { return; }` — untouched, still the first line of the function.
   - Re-import idempotency — preserved: the id is deterministic per statement (same file → same `first` content → same hash → same id), so `INSERT OR REPLACE` still updates the same row in place on re-import. Proven by test (see below).
   - Existing transaction ids — untouched: the change only affects the *new* row this function inserts at index 0; it never touches `txns[0]`'s (now index 1) own id. Proven by test.
   - Export behavior — unaffected: no export code reads `Transaction.id`; verified in the design phase.
   - Dedup behavior — unaffected: `Transaction::hash()` itself is unchanged (read-only call, not modified); the `dedupe_hashes` table and `detect_duplicates` already exclude opening-balance rows entirely.

## Tests added

**Unit tests** (`src/parser/excel_parser.rs`, 4 new, in the existing `prepend_opening_balance_row` test group):
- `prepend_ob_id_is_no_longer_the_bare_literal` — guards against reverting the fix.
- `prepend_ob_id_is_identical_across_repeated_imports_of_the_same_statement` — **same statement twice → same id**.
- `prepend_ob_id_differs_for_two_different_statements` — **two different statements → different ids**.
- `prepend_ob_does_not_change_existing_transaction_ids` — real transaction ids at index 1+ are untouched by the OB-row insertion.

**Integration test** (`tests/analytics_export_persistence.rs`, 1 new): `reloading_an_older_import_still_restores_its_own_opening_balance` — reproduces the exact bug end-to-end using two different real fixtures (SBI.pdf, Kotak Bank.pdf) for one client: imports both, then calls `db::get_transactions_for_import` for the *older* import (the precise "Reload Import" code path in `main.rs`) and confirms it still finds its own correct opening balance, distinct from the newer import's.

All 5 new tests pass; all pre-existing tests continue to pass unmodified.

## Quality gate

- `cargo fmt` — clean (new lines formatted; pre-existing files with this codebase's established aligned-field style, e.g. the rest of `excel_parser.rs`, deliberately left unreformatted, consistent with this session's standing policy).
- `cargo clippy --all-targets --all-features` — clean, 0 errors. One pre-existing warning in `excel_parser.rs` (line 558, an unrelated loop-index lint in a different function) predates this change and was left untouched per "fix only what you changed."
- `cargo build` — clean.
- `cargo test` — **524 passing** (482 lib + 8 bin + 33 integration + 1 doctest), **4 ignored** (pre-existing, unrelated PDF-extraction bugs from the earlier integration-test phase), **0 failing**.

## Result

The confirmed bug (blank Opening Balance on "Reload Import" for any statement other than a client's most recent one, and that blank value leaking into exports) is fixed for all imports made going forward. Per the design document: data already lost to a collision *before* this fix ships remains unrecoverable — this stops future collisions, not past ones.

No commit made yet — stopping for review as requested.
