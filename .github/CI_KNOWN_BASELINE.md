# CI known baseline

`ci.yml` runs `fmt`, `clippy`, and `test` across the full workspace (desktop
crate + `server/` + `protocol/`) on every push to `main` and every pull
request, with no scope reduction and no weakened lint level — `cargo fmt
--all -- --check` and `cargo clippy --workspace --all-targets --all-features
-- -D warnings` are both enforced exactly as written, workspace-wide.

## Known, accepted failures on the current baseline

As of this workflow's introduction, the **desktop crate** (`bank-statement-processor`,
i.e. everything outside `server/` and `protocol/`) does not pass either check
cleanly:

- **`fmt`**: the desktop crate does not follow default `rustfmt` output —
  no `rustfmt.toml` exists, and the codebase deliberately hand-aligns struct
  field declarations (e.g. `pub idx:         usize,`) and similar spacing
  that default `rustfmt` collapses. Running `cargo fmt --all -- --check`
  reports diffs across roughly a thousand pre-existing locations, unrelated
  to any single change.
- **`clippy`**: the desktop crate carries on the order of 100 pre-existing
  lint warnings under `--all-features` (loop-index patterns, some
  clone/`&mut Vec` idioms, etc.) that predate this CI workflow.

**`server/` and `protocol/` are clean** — zero `fmt` diffs, zero `clippy`
warnings, on this same baseline.

## Why the checks are still enforced as-is, not scoped down

CI intentionally does **not** exclude the desktop crate from `fmt`/`clippy`,
and does **not** relax `-D warnings` or drop `--check`. Scoping the checks
down to only the already-clean crates would hide the debt instead of making
it visible, and would mean a future regression in the desktop crate's *own*
lint/format state — even without touching `server`/`protocol` — is not
caught. The gate is deliberately strict from day one; the pre-existing
`fmt`/`clippy` failures are the honest starting point, not a workflow bug.

## Plan

This debt is tracked for a **separate cleanup phase**, not fixed as part of
introducing CI. Until that phase lands, `fmt` and `clippy` are expected to
fail on `main` for reasons unrelated to whatever change triggered the run —
check whether a given failure is inside `server/`/`protocol/` (a real
regression, must be fixed) or the desktop crate outside `server/`/`protocol/`
(pre-existing, tracked here) before treating a red run as a blocker for the
change under review.
