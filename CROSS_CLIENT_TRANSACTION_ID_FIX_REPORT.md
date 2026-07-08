# Cross-Client Transaction ID Data-Corruption Fix

**Date:** 2026-07-08
**Severity:** Critical (silent cross-tenant data corruption in a multi-client accounting tool)
**Status:** Fixed, tested, quality gate clean.

---

## 1. Root cause

### 1.1 The schema defect

`transactions.id TEXT PRIMARY KEY` (`src/db/mod.rs`, base schema) was the table's **sole** primary key — globally unique across every client in the database, not scoped per client.

### 1.2 ID generation has no client or file salt

Every non-test id-generation site produces a purely **positional** id — a function of in-file row index (and sometimes a running counter), with nothing tying it to a specific client or file:

| File : line | Pattern |
|---|---|
| `src/parser/excel_parser.rs:735` | `format!("t_{}_{}", i, txns.len())` |
| `src/parser/excel_parser.rs:743` | `format!("t_{}", i)` |
| `src/parser/ocr_parser.rs:217` | `format!("t_ocr_{}", txn_counter)` |
| `src/parser/transaction_extractor.rs:300` | `format!("t_fw_{}_{}", i, txn_counter)` |
| `src/parser/transaction_extractor.rs:468` | `format!("t_cosmos_{}_{}", i, txn_counter)` |

### 1.3 The guaranteed case: the opening-balance row

`src/parser/excel_parser.rs:502`'s `prepend_opening_balance_row` — shared unconditionally by **all three** import pipelines (Excel, PDF, OCR; `pdf_parser.rs:104,514`, `ocr_parser.rs:257`) — hardcodes the synthetic opening-balance row's id to the **literal string `"opening_balance"`**, for every single import, for every client, every time. This is not a "plausible collision," it is a **100%-guaranteed** collision the moment any second client (or the same client's second import) writes an opening-balance row.

### 1.4 The write path that turns a collision into corruption

`db::upsert_transactions` (`src/db/mod.rs:148`) writes via `INSERT OR REPLACE INTO transactions (id, client_id, ...)`. Because `id` alone was the primary key, this **replaces** — not inserts alongside — any existing row with a matching id, silently changing that row's `client_id` and every other column to the new writer's data. Each subsequent client's import of any file was silently stealing and reassigning the previous opening-balance row (and any other id-colliding row) to itself.

### 1.5 Functions that assumed per-client scoping without enforcing it

Three functions updated/deleted rows by bare `id`, with no `client_id` parameter to constrain the `WHERE` clause at all:

| Function | File : line | Old query |
|---|---|---|
| `upsert_transaction_classification` | `db/mod.rs:306` | `UPDATE transactions SET ... WHERE id=?` |
| `update_dup_flags` | `db/mod.rs:324` | `UPDATE transactions SET dup_flag=? WHERE id=?` |
| `delete_transaction` | `db/mod.rs:334` | `DELETE FROM transactions WHERE id=?` |

Under the old schema these were merely redundant (only one row could ever exist per id anyway), but they were **not defense-in-depth** — nothing stopped them from silently updating/deleting the wrong client's row the moment a collision existed. A fourth site, the migration importer's `count_existing_transaction_ids` (`src/migration/importer.rs:376`), queried `WHERE id IN (...)` with no client scope at all, causing cross-client false-positive duplicate detection during data migration.

### 1.6 Verified NOT affected (no changes needed)

- **`Transaction::hash()`** (`src/parser/mod.rs:187`) — content-based (date/narration/debit/credit), used for cross-session dedupe via the separately, already-correctly `client_id`-scoped `dedupe_hashes` table.
- **`classifier::detect_duplicates`** (`src/classifier.rs:494`) — purely in-memory, operates on a slice already scoped to one client's session; takes no `Connection`/`client_id` at all.
- **All export logic** (`src/export/*.rs`) — never reads `Transaction.id`; works entirely off the other fields.
- **`ai_classifier.rs`** — operates on in-memory data only.
- **`get_transactions`, `get_transactions_for_import`, `delete_transactions_for_client`** — already correctly scoped by `client_id`/`import_id` in their `WHERE` clause; they don't have the "ambiguous row" problem since they never look up by bare `id`.

---

## 2. The fix

### 2.1 Design choice: schema-level composite primary key, not id-generation salting

Considered salting generated ids with `client_id` instead of a schema change. Rejected: it would require threading `client_id` through every parser function (a much larger, riskier surface — `pdf_parser`/`excel_parser`/`ocr_parser`/`transaction_extractor` — than the DB layer), it wouldn't guarantee true uniqueness on its own, and critically it provides **no defense in depth** — any future or legacy-migration-imported id (see `migration/transformer.rs`, which preserves the *old app's own* ids verbatim) is outside the parser's control to keep collision-free. A schema-level fix protects against every id-generation path at once, current and future.

**Fix:** `transactions`' primary key is now the composite `(client_id, id)`. The same literal id can now correctly coexist across different clients; a genuine duplicate `(client_id, id)` pair is still rejected, preserving the existing "re-importing the same file for the same client is idempotent, not duplicated" behavior.

### 2.2 Why this doesn't break re-import idempotency

The primary key is `(client_id, id)`, **not** `(client_id, import_id, id)`. Re-importing the identical file for the same client still produces the same `id`s under the same `client_id`, so `INSERT OR REPLACE` still treats it as "update the existing row," not a duplicate — exactly the existing, tested, intentional behavior (`tests/analytics_export_persistence.rs::re_importing_the_same_real_statement_does_not_duplicate_rows`, unchanged and still passing).

---

## 3. Files changed

| File | Change |
|---|---|
| `src/db/mod.rs` | New migration 5: rebuilds `transactions` with `PRIMARY KEY (client_id, id)`. `upsert_transaction_classification`, `update_dup_flags`, `delete_transaction` now take and enforce `client_id`. Fixed one pre-existing test (`migration_4_dedupes_...`) whose fixture hand-simulated `user_version=3` via base schema alone instead of really running migrations 1-3 — harmless before, but migration 5 (correctly) assumes migration 1/3's columns already exist by the time it runs. Added 7 new tests. |
| `src/migration/importer.rs` | `count_existing_transaction_ids` now takes and filters by `client_id`, preventing cross-client false-positive duplicate detection during legacy-data migration. |
| `src/main.rs` | 4 call sites updated to pass `client_id`/`cid` through to the 3 now-scoped functions; the Undo handler's classification-persist now correctly skips (with a log warning) rather than unconditionally calling with no client context if `client_id` is `None`. |
| `tests/analytics_export_persistence.rs` | `multi_client_transaction_data_is_fully_isolated` — previously a documented `#[ignore]`d bug report — now un-ignored, passes, and runs permanently as a regression test. |
| `CROSS_CLIENT_TRANSACTION_ID_FIX_REPORT.md` | This report. |

**Not changed, verified unnecessary:** `SCHEMA_SQL` (matches this codebase's established convention — schema evolution happens entirely via `MIGRATIONS`, uniformly for fresh installs and upgrades; `SCHEMA_SQL` intentionally stays frozen at its original pre-migration-1 shape). Parser id-generation code. Export code. `Transaction::hash()`/dedupe. `detect_duplicates`.

---

## 4. Migration details

Migration 5 (`src/db/mod.rs`), the first migration in this codebase to rebuild a table rather than `ALTER TABLE`/`CREATE TABLE IF NOT EXISTS` (SQLite cannot alter a primary key in place):

```sql
BEGIN TRANSACTION;
CREATE TABLE transactions_new ( ... same columns ..., PRIMARY KEY (client_id, id) );
INSERT INTO transactions_new (<all columns>) SELECT <all columns> FROM transactions;
DROP TABLE transactions;
ALTER TABLE transactions_new RENAME TO transactions;
CREATE INDEX ... (all 5 original + 1 new: idx_txn_import on import_id);
COMMIT;
```

Wrapped in an explicit `BEGIN`/`COMMIT` — every other migration in `MIGRATIONS` is a single atomic `ALTER`/`CREATE` statement and doesn't need one, but a multi-statement table rebuild does: a failure partway through must not leave the database with a half-built replacement table and no working `transactions` table at all.

Moving from a stricter (globally-unique) constraint to a looser (per-client-unique) one can never introduce a new constraint violation in existing data, so the `INSERT INTO ... SELECT` is guaranteed to succeed against any pre-existing database. Runs automatically on next app launch via the existing versioned-migration framework (`PRAGMA user_version`); no user action required.

---

## 5. Backward compatibility

- **Existing databases upgrade automatically**, preserving 100% of their current (post-collision) row content — verified by `migration_5_preserves_all_existing_single_client_transactions`.
- **No external behavior changes** for any single-client-at-a-time-loading workflow: `db::upsert_transactions`'s call signature is unchanged (it already took `client_id`); re-import idempotency is unchanged and tested.
- **Important, honest caveat — this migration cannot recover data already lost.** If a database was corrupted by this bug *before* upgrading (a row silently reassigned to the wrong client — most commonly, every client's opening-balance row except whichever one wrote it last), that data is gone by the time migration 5 runs: only the most recent writer's version of that row exists anywhere in the old table to migrate forward. This migration stops **future** collisions; it cannot undo **past** ones.

---

## 6. Test coverage

**8 tests directly prove/guard this fix** (all passing):

| Test | Proves |
|---|---|
| `db::tests::migration_5_preserves_all_existing_single_client_transactions` | Migration | Existing data survives the rebuild |
| `db::tests::migration_5_allows_the_same_literal_id_for_two_different_clients` | **Same id, different clients, must not overwrite** | Schema-level fix, direct reproduction of the exact opening-balance scenario |
| `db::tests::migration_5_rejects_the_same_id_twice_for_the_same_client` | Migration didn't over-correct | Genuine same-client duplicate still rejected |
| `db::tests::migration_5_is_idempotent_when_run_twice` | Migration safety | Re-running on every app launch is a safe no-op |
| `db::tests::upsert_transaction_classification_does_not_touch_a_different_clients_row_with_the_same_id` | Function-level fix | Edit/Undo doesn't cross-contaminate |
| `db::tests::delete_transaction_does_not_touch_a_different_clients_row_with_the_same_id` | Function-level fix | Delete doesn't cross-contaminate |
| `db::tests::update_dup_flags_does_not_touch_a_different_clients_row_with_the_same_id` | Function-level fix | Dedupe-reset doesn't cross-contaminate |
| `tests/analytics_export_persistence.rs::multi_client_transaction_data_is_fully_isolated` | **Regression test for this exact bug** | End-to-end, via the public `db::` API, real fixture data — was `#[ignore]`d documenting the bug, now passes for real |

Plus existing tests confirming **existing imports continue to work** unmodified: `re_importing_the_same_real_statement_does_not_duplicate_rows`, `real_transactions_persist_and_reload_identically` (both still pass, unchanged).

**Full suite:** 523 total test functions (478 lib + 8 bin + 36 integration + 1 doctest), 519 passing, 4 ignored (pre-existing, unrelated PDF text-extraction bugs documented in the prior integration-test phase — not part of this fix), 0 failing.

`cargo fmt`, `cargo clippy --all-targets --all-features`, `cargo build`, `cargo test` — all clean.

---

## 7. Remaining risks

1. **Same-client, cross-import opening-balance collision — separate, pre-existing, NOT fixed here.** The opening-balance row's id is still the bare literal `"opening_balance"`. The composite key `(client_id, id)` fixes the *cross-client* case (what was asked), but a single client importing a **second** statement still collides with their own **first** statement's opening-balance row under this same literal id, silently overwriting it via `INSERT OR REPLACE`. This is a distinct bug from the one fixed here (same client, not different clients) and was intentionally left out of scope — fixing it would mean changing opening-balance id generation to be period/file-specific, a behavioral change beyond "ensure identity is scoped correctly per client," and risks its own regressions if rushed. Recommend a dedicated follow-up.
2. **Positional ids can still collide within the same client across different imports** for regular (non-opening-balance) rows too, if two different files happen to produce the same row-index/count pattern — same underlying cause as #1, same recommendation.
3. **Pre-upgrade data loss is unrecoverable**, as stated in §5 — cannot be mitigated retroactively; only communicated.

---

## 8. Commit

This fix is committed separately from all other work, with a message summarizing the root cause and fix.
