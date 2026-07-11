# License System Design — Phase 3A

**Status:** Architecture + desktop-side implementation. No payment integration. No real server exists yet (API is specification-only — see `API_SPECIFICATION.md`).
**Scope:** This document covers the licensing/subscription architecture only. Payment gateway (Razorpay) integration is explicitly deferred to a later phase per the task instructions.

---

## 1. Relationship to the existing auth system

This codebase already has `src/auth/` — a monthly HMAC-derived password gate (`monthly_password.rs`), explicitly documented in its own module doc as **"a licensing/anti-piracy gate, not an access-control boundary."** It is **not modified, removed, or replaced by this phase**. Per the task rules ("do not rewrite or refactor unrelated code," "all existing functionality must continue to work exactly as before"), it stays exactly as-is.

The new subscription/license system is **additive**: a separate module (`src/license/`), separate database tables (migration 6), and a separate startup hook. The two systems currently answer different questions:

- `auth::validate_credentials` — "does this login screen accept this email/password this month?" (existing, unchanged).
- `license::check_status` — "is this installation's subscription currently valid?" (new, this phase).

**They are not wired together in this phase.** See §7 for why, and what "wiring together" would mean later.

## 2. Core entities (desktop-side view)

| Field (from the task spec) | Where it lives on the desktop |
|---|---|
| User Account | Not stored locally beyond the activation email (see `LICENSE_DATABASE_SCHEMA.md` — `users` is a **server-side** table; the desktop only ever sees the fields the server chooses to return) |
| Customer ID | Cached locally after activation (`local_license.customer_id`) |
| License ID | Cached locally (`local_license.license_id`) |
| Subscription Type | Cached locally (`local_license.subscription_type`: `trial` \| `monthly` \| `yearly` \| `lifetime`) |
| License Status | Cached locally (`local_license.status`), re-derived on every check (see §4) |
| Expiry Date | Cached locally (`local_license.expires_at`) |
| Last Validation Time | Cached locally (`local_license.last_validated_at`) — the anchor for offline grace period math |
| Offline Grace Period | Configurable, defaults to 7 days (`local_license.grace_period_days`), server-supplied at activation/validation time, not hardcoded in the client |
| Device ID | Generated once per installation, persisted locally (`device_info.device_id`) — see §5 |
| Machine Fingerprint | Derived from stable local machine properties, persisted and re-checked (`device_info.fingerprint`) — see §5 |

The **source of truth** for all of this is the server (once it exists). The desktop's copy is a cache that lets the app keep working offline for a bounded window — never the authority.

## 3. Survival across reinstall / reboot / update

- **Reboot:** trivially survives — all state is in the SQLite database file on disk (`bsp_data.db`, the same file every other feature already persists to), not in memory.
- **Software update:** survives via the existing versioned-migration framework (`db::MIGRATIONS`, `PRAGMA user_version`) — the new tables are added by migration 6, applied automatically on first launch after upgrade, exactly like every other schema change in this codebase (see `LICENSE_DATABASE_SCHEMA.md` §3 for the exact migration).
- **Reinstall (same machine):** if the installer/uninstaller leaves `bsp_data.db` in place (this app installs to a user-writable directory next to the executable, not a location an uninstaller typically wipes — verified against the existing `db_path` computation in `main.rs`, `current_exe()`'s directory), the license survives automatically, same as reboot. If the database file itself is deleted, the device has no local record and must re-activate against the server (by design — see §7, this is not a bug, it's the same trust boundary that makes a *copied* database not silently grant a license, §6).
- **Reinstall (different machine) / migration to new hardware:** requires a fresh `activate-license` call — the server is expected to support "deactivate old device, activate new device" as an explicit customer action (documented in `API_SPECIFICATION.md`, `POST /activate-license`), not something the desktop client can silently infer.

## 4. Validation flow

Implements the flow given in the task exactly, as `license::check_status(conn, client) -> LicenseStatus`:

```
Desktop App startup
        │
        ▼
Read local license (local_license table)
        │
   no local record? ──────────────────────────► NotActivated
        │
        ▼
Internet available? ──── no ────────────────────┐
        │ yes                                    │
        ▼                                        │
Call POST /validate-license                      │
        │                                        │
   succeeds, valid ──────► Active                │
   succeeds, invalid ────► Expired/Suspended      │
   network/server error ──┐                       │
        │                 │                       │
        ▼                 ▼                       ▼
                  Check offline grace period (now - last_validated_at)
                          │
                 within grace_period_days? ── yes ──► Active (offline-grace)
                          │ no
                          ▼
                       Expired
```

`LicenseStatus` (the module's central type) is one of: `NotActivated`, `Active`, `ActiveOfflineGrace { days_remaining }`, `GracePeriodExpired`, `Suspended`, `Expired`. Every branch above maps to exactly one of these — there is no "unknown, assume valid" fallthrough (see `LICENSE_SECURITY_REVIEW.md` for why fail-open is rejected).

## 5. Device ID and machine fingerprint

Two distinct concepts, deliberately:

- **Device ID** — a random UUID v4, generated once on first activation, stored in `device_info.device_id` and never regenerated. Identifies *this installation* to the server. Cheap, portable, no OS dependency.
- **Machine fingerprint** — a SHA-256 hash of a small set of environment-derived, moderately-stable values (computer name, username, processor identifier — see `LICENSE_SECURITY_REVIEW.md` §2 for the exact inputs and their honest strength/weakness). Its purpose is narrower than the device ID: detecting when a `local_license` row (or the whole database file) has been *copied* to different hardware rather than genuinely reinstalled on the same machine (§6).

The fingerprint is intentionally **not** used as the primary identity (that's the device ID) — it's a secondary consistency check, because environment-variable-derived fingerprints can legitimately change on the *same* machine (username changes, computer renamed) and the system must not brick a legitimate user over that. A fingerprint mismatch is logged and reported to the server on the next successful validation call (server-side policy decision, not a local hard block) — see `LICENSE_SECURITY_REVIEW.md` §2.

## 6. Offline mode

- `local_license.last_validated_at` is updated only on a *successful* server validation (or successful activation).
- Every app-visible "is licensed" check computes `days_since_last_validation = now − last_validated_at` and compares against `grace_period_days` (server-supplied, default 7 — see `LICENSE_DATABASE_SCHEMA.md`).
- The app **never requires internet on every launch** — a successful validation within the grace window is sufficient, matching the task's explicit requirement.
- Clock-rollback protection: see `LICENSE_SECURITY_REVIEW.md` §1 — a naive `now − last_validated_at` is trivially defeated by turning the system clock back, so the desktop also tracks a monotonically-non-decreasing "highest timestamp ever observed" and treats any wall-clock read *behind* that watermark as suspicious (fails closed, not open).

## 7. Deliberate scope decision: no startup gate yet

**The license status check is implemented and wired into startup, but it does not block login or app usage in this phase.** This is a considered decision, not an oversight, for three concrete reasons:

1. **No real server exists yet.** `API_SPECIFICATION.md` is a specification, not a running service. Every `validate-license` call would hit an unreachable endpoint, meaning every installation would immediately fall through to "no internet" → offline grace period → eventual hard expiry, with no way to renew (no payment integration exists).
2. **No activation path exists yet.** There is no `POST /activate-license` server to call, so `local_license` starts empty for every installation, including the developer's own. A hard gate today would lock out the only real user of this software before the system that lets them un-lock it exists.
3. **The task's own sequencing** ("do not start Payment Gateway implementation until this licensing architecture is complete and approved") implies staged rollout with an approval gate — flipping this to a hard block is a one-line, reversible follow-up (`license::should_enforce()` in `src/license/mod.rs` is the single call site to change from `false` to `true`) once a real server and payment path exist. Building it as an explicit, isolated switch rather than baking enforcement into the login handler directly is itself part of the design, not a shortcut.

What *is* live today: `check_status` runs on every startup, is logged, and its result is stored in `AppState` for future UI use (e.g., a status banner) — the machinery is real and tested, just not load-bearing yet.

## 8. Subscription types

`trial | monthly | yearly | lifetime` — modeled as a plain string-backed enum server-side and client-side (not a fixed-cardinality SQL `CHECK` list on the desktop), specifically so a new type (e.g., a future "lifetime" tier, explicitly requested as future-ready) doesn't require a desktop migration to become representable — only a server-side change and a client update to *price/display* it specially. An unrecognized subscription type string is treated as "licensed, unknown tier" rather than rejected, so an older client build doesn't break on a newer server introducing a new tier.

## 9. Interfaces for future payment integration

`src/license/client.rs` defines a `LicenseApiClient` trait (the 7 endpoints from the task spec as trait methods) with:
- `OfflineClient` — the only implementation that exists today; every method returns `Err(ApiError::NoServerConfigured)` immediately, no network I/O attempted. This is what the app actually runs against right now.
- A commented extension point documenting exactly where a future `HttpLicenseClient` (using the already-present `reqwest` dependency) and, later, a Razorpay webhook-driven payment flow plug in — without touching `license::mod`'s public API or any call site in `main.rs`.

See `API_SPECIFICATION.md` for the full request/response contract this trait is designed against.
