# Design Document: Same-Client Opening-Balance Collision

**Status:** Design only — no code changes. Awaiting approval before implementation.
**Relates to:** `CROSS_CLIENT_TRANSACTION_ID_FIX_REPORT.md` §7, "Remaining risks" item 1 (a distinct, separate bug from the cross-client fix already shipped in commit `f138f2e`).

---

## 1. Exact root cause

The synthetic "opening balance" row that every import prepends to its transaction list is assigned a **hardcoded, non-varying id**:

`src/parser/excel_parser.rs`, `prepend_opening_balance_row` (~line 502):
```rust
let ob_row = Transaction {
    id: "opening_balance".to_owned(),
    ...
};
```

This function is shared, unconditionally, by all three import pipelines:
- `pdf_parser.rs:104,514`
- `excel_parser.rs` (its own call site, same file)
- `ocr_parser.rs:257`

Since the fix in `f138f2e`, `transactions`' primary key is `(client_id, id)`. This closed the **cross-client** collision, but the opening-balance row's id is still constant **within** a client across every import that client ever makes. When client X imports statement A (January), then later imports statement B (February):

- Both produce a row with the identical composite key `(X, "opening_balance")`.
- `db::upsert_transactions` writes via `INSERT OR REPLACE INTO transactions (id, client_id, import_id, ...)`.
- February's `INSERT OR REPLACE` replaces the **entire row**, including `import_id` — it now points at February's import, not January's.
- There is only ever **one physical opening-balance row per client**, no matter how many statements they've imported. Every import except the most recent one silently loses its opening-balance row.

This is the same root defect class as the cross-client bug (a non-unique, non-salted id under `INSERT OR REPLACE`), one level narrower in scope (same client, different import — not different clients).

## 2. Confirmed user-visible impact

Traced precisely (not assumed) via `main.rs`:

- **Fresh import is unaffected.** `push_dashboard`/`calc_closing` use `ParseResult.opening_balance` — a plain `Option<f64>` field, computed in-memory at parse time, never round-tripped through the DB. This field exists *separately* from the synthetic `Transaction` row (see §4, relevant to the recommended fix).
- **"Reload Import" is broken** (`on_do_reload_import`, `main.rs:4056-4089`). It calls `db::get_transactions_for_import(conn, import_id)` for the *specific* selected import, then derives opening balance via `.find(|t| t.is_opening_balance)`. For any import except the client's most recent one, this now finds nothing (the physical row's `import_id` points elsewhere). Result: **the dashboard's "Opening Balance" card shows blank for any reloaded import older than the client's latest.**
- **Exports done in a reloaded session inherit the blank value** — `main.rs:2504-2640` (Excel/CSV/Tally/generic-XML export) all read the in-memory session's `opening_balance`, which was just set to blank by the broken reload.
- **Switching clients / "Load All"** (`main.rs:3368,3838-3840`) scans the client's *entire* history for an `is_opening_balance` row — finds the one surviving row (the latest import's), so this path is self-consistent but silently loses the ability to ever show an older period's true opening balance again.
- **Closing balance is unaffected** — `analytics::compute`'s `closing_bal` derives from the last real transaction's stored balance, not from the opening-balance row.

No evidence the schema/architecture was ever designed to hold multiple per-import opening balances — `import_history` has no such column, and `get_transactions_for_import`'s reliance on finding an `is_opening_balance` row was an implicit, apparently-unverified assumption.

## 3. Affected files (if fixed)

| File | Role |
|---|---|
| `src/parser/excel_parser.rs` | `prepend_opening_balance_row` — the actual defect; shared by all 3 pipelines |
| `src/parser/pdf_parser.rs`, `src/parser/ocr_parser.rs` | Callers only — no change needed themselves (see §5) |
| `src/db/mod.rs` | Possibly: new migration, if the schema-column approach (Option C) is chosen instead of the recommended id-derivation approach |
| `src/main.rs` | Read side (`on_do_reload_import` and friends) — needs no change under the recommended option; would need changes under Option C |
| Tests | New unit tests (id-generation) + at least one DB-level integration test reproducing "Reload Import shows correct opening balance for an older import after a newer one exists" |

**Verified NOT affected:** nothing else in the codebase depends on the *literal string* `"opening_balance"` — every consumer checks the `is_opening_balance: bool` flag, not the id value (confirmed via full-repo grep). Changing what string the id holds is safe with respect to this.

## 4. All possible solutions considered

### Option A — Widen the table's primary key to `(client_id, import_id, id)`
Rejected outright, not merely deprioritized: this was already explicitly considered and rejected during the cross-client fix. Widening the PK to include `import_id` would break **regular transaction rows'** re-import idempotency — re-importing the identical statement generates a *new* `import_id` each time (`save_import` is `AUTOINCREMENT`), so the same transaction would be treated as brand-new instead of "update in place," silently duplicating every row on every re-import. This is a schema-wide change with a severe, unacceptable side effect on already-correct behavior.

### Option B — Derive the opening-balance row's id from stable, per-period content (RECOMMENDED)
Change `prepend_opening_balance_row` to generate the id from something that is:
- **stable across a re-import of the identical file** (preserves existing idempotency), and
- **different across genuinely different statements/periods** (fixes the collision).

Concretely: derive from the first real transaction's content, e.g. `format!("opening_balance_{}", txns[0].hash())`, reusing the already-implemented, already-tested `Transaction::hash()` (date + narration + debit/credit). `prepend_opening_balance_row` already guarantees `txns` is non-empty before reaching this point (it early-returns on `txns.is_empty()`), so `txns[0]` is always available.

No schema change: the `(client_id, id)` composite key from the cross-client fix already provides correct scoping once the id itself stops being a constant — a different id per period naturally becomes a different row under the existing schema.

### Option C — Move opening balance onto `import_history` as its own column
Add `opening_balance REAL` to `import_history` (schema migration required), pass `ParseResult.opening_balance` — which is *already computed separately* from the synthetic transaction row — into `save_import` directly, and change the read side (`on_do_reload_import` and any other `.find(is_opening_balance)` site) to read `import_history.opening_balance` for the relevant `import_id` instead of hunting through `transactions`.

Architecturally the "more correct" long-term model (opening balance is a property of a *period/import*, not a transaction), and sidesteps the id-collision question entirely for this data. But materially larger: requires a migration, a `save_import` signature change (3+ call sites: `main.rs` single-file load, `main.rs` batch import, `migration/importer.rs`), a new/changed getter, and changes to every current `.find(|t| t.is_opening_balance)` read site — plus a decision about whether the synthetic transaction row should still be written at all (removing it outright risks breaking any UI code that expects to see an opening-balance row *inline in the transaction list/table*, which was not audited as part of this design pass and would need to be before removing it).

### Option D — Accept multiple opening-balance rows per client via a non-deterministic id (e.g. random/uuid, or timestamp-based)
Rejected: a non-deterministic id would fix the collision but **break re-import idempotency** — re-importing the identical file would mint a new id each time and create a duplicate opening-balance row per re-import, rather than updating the existing one in place. The id must be *deterministic per period*, which is exactly Option B, not this.

### Option E — Partial/separate uniqueness constraint scoped by `import_id` only for opening-balance rows
Considered and folded into Option B: any mechanism that keeps `INSERT OR REPLACE` semantics working correctly still requires the row's actual `id` (or full key) to vary per period — there's no SQLite construct that makes `INSERT OR REPLACE` respect a different, narrower uniqueness rule for a subset of rows without the underlying id itself differing. Not a materially distinct option from B.

## 5. Recommended solution

**Option B.** It fully fixes the confirmed bug (§2), requires no schema migration, touches exactly one function (whose fix applies to all three import pipelines "for free," since they all share it), requires no changes to any read-side code (`is_opening_balance`-flag-based reads already work correctly the moment the row stops being overwritten), and carries the lowest risk of collateral regression. Option C is the architecturally cleaner long-term model but is materially larger in scope, touches more files, needs its own migration, and — per this project's own established preference this session ("do not change external behaviour unless necessary," "do not rewrite working modules") — is more than what's needed to close this specific bug.

## 6. Risks

1. **Residual theoretical collision:** two genuinely different statements for the same client whose *first real transaction* happens to have identical date + narration + debit/credit would still collide under Option B. Extremely low probability in practice (this is the same collision surface `Transaction::hash()` already accepts for cross-session dedupe today), not considered a practical blocker.
2. **Past data is unrecoverable, as before.** This fix only prevents *future* collisions. Any client who has already imported a second statement before this fix ships has already lost their first statement's opening-balance row; there is nothing left to migrate or restore.
3. **Derivation-formula stability going forward:** if the id-derivation formula is changed again later without care, a previously-stored opening-balance row could be silently orphaned (old id) rather than updated (since the "same file" would now compute a different id) — worth a code comment warning future maintainers, not a risk to this change itself.
4. **`import_history.file_hash`** exists as a column but is genuinely vestigial (never written or read anywhere) — it looked like a promising ready-made per-file identifier but is not actually populated, so it isn't a viable shortcut without first wiring it up, which is out of scope.

## 7. Backward compatibility

- No schema/migration impact — existing databases need no upgrade step for this fix.
- No API/behavior change for any code path that already works correctly today (fresh imports, cross-client isolation, regular-transaction re-import idempotency all remain exactly as they are).
- The **only** behavior change: previously, a client's older imports' "Reload Import" silently showed a blank opening balance; after the fix, it will show the correct value **for any import made after the fix ships**. Imports made *before* the fix remain unrecoverable (§6.2) — reloading them will still show blank, because their opening-balance row was already overwritten prior to upgrading. This is a strict improvement with a clearly-bounded, honestly-stated limit, not a full retroactive repair.

## 8. Is a schema migration required?

**No**, under the recommended Option B. The composite primary key `(client_id, id)` already exists from the prior fix; this change only alters what string is computed for `id` on newly-created opening-balance rows going forward.

(Option C, not recommended, would require one.)

## 9. Estimated implementation complexity

**Low.** Core change is a few lines in one function (`prepend_opening_balance_row`). Main effort is test coverage:
- A unit test proving two different (synthetic) statements for the same client produce different opening-balance ids.
- A unit test proving re-parsing the identical statement produces the same id (idempotency preserved).
- A DB-level integration test reproducing the exact bug end-to-end: import statement A, then statement B, for the same client, then confirm `get_transactions_for_import(import_a_id)` still finds statement A's opening-balance row (this is the direct regression test for the "Reload Import" breakage found in §2).

Estimated at roughly half the effort of the cross-client fix (no migration, no multi-file call-site changes, no `db.rs` function signature changes) — realistically achievable in a single focused implementation pass.

---

**No code has been modified.** Waiting for approval before implementing.
