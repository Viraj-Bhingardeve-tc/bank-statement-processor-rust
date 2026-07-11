# License Security Review — Phase 3A

**Ground rule, stated once and applied throughout this document:** the desktop app is never trusted. Every status this document describes the client computing locally is a **convenience for offline operation**, not the authority. The server (once built) is the only party that can *grant* a license; the client can only *cache* and *check* what it was last told, and must fail closed when it can't tell.

This document is written in the same spirit as this codebase's existing `src/auth/monthly_password.rs` module doc — it states what each protection actually achieves and, just as importantly, what it does **not**, rather than overselling client-side controls that are inherently defeatable by anyone with the binary.

---

## 1. Clock rollback

**Attack:** turn the system clock back before it reaches `expires_at`, or before the offline grace period elapses, so `now − last_validated_at` never grows.

**Protection implemented:** `local_license.highest_seen_clock` — a watermark updated to `max(highest_seen_clock, current_time)` on every single license check, online or offline. Before computing any expiry/grace-period math, the client compares the current wall-clock reading against this watermark:

- `current_time >= highest_seen_clock` → normal, proceed with the real grace-period computation.
- `current_time < highest_seen_clock` → the clock has moved backward since this app last ran. Treated as **`LicenseStatus::GracePeriodExpired`** immediately (fail closed), regardless of what the naive `now − last_validated_at` math would say, and logged to `license_validation_log` with `detail = "clock rollback detected"`.

**Honest limitation:** this only detects rollback *relative to what this installation has itself already observed*. A rollback performed *before* the very first license check ever ran on this machine (i.e., before `highest_seen_clock` has any real value) is invisible to this specific mechanism — mitigated in practice by `activate-license` requiring a live server round-trip (the server's clock, not the client's, sets the initial state), so there's no meaningful "before the first check" window for an installation to have exploited. A user who never lets the app reach the internet even once cannot be fully protected against clock rollback by the client alone; the online `/validate-license` path (server-side `client_clock` logging, `API_SPECIFICATION.md`) is the real backstop, and is a server-side detection concern, not something the desktop can fix unilaterally.

## 2. Copied license file / copied database

**Attack:** copy `bsp_data.db` (or just the `local_license`/`device_info` rows) from a licensed machine to an unlicensed one.

**Protection implemented:** `device_info.device_id` + `device_info.machine_fingerprint` are checked together on every validation, online or offline. A copied database carries the *original* machine's `device_id` and fingerprint. Two independent server-side checks then apply once a server exists:
1. `devices.device_id` is unique per `license_id` — the server already has this exact `device_id` recorded as belonging to the original device. A second machine presenting the same `device_id` but a **different** `machine_fingerprint` in `/validate-license` is a strong forgery signal (`fingerprint_matched: false` in the response).
2. Purely offline (no server reachable), the copy is *not detected by this alone* — this is an honest gap, not a fixed one. The offline grace period exists precisely to bound how long this gap matters: a copied database can ride the grace period (default 7 days) before the next mandatory-feeling online check, then is caught the moment either machine successfully reaches the server (whichever validates last "wins" the `device_id`; the other starts failing fingerprint checks).

**This is the same class of limitation the codebase already documents for the monthly-password gate** (`auth/monthly_password.rs`'s module doc): a purely offline-capable client cannot be made airtight against file copying without *requiring* an online check on every single launch, which the task explicitly rules out ("never require internet on every launch"). The design accepts a bounded (grace-period-sized) exposure window in exchange for genuine offline usability, and documents that trade-off here rather than silently accepting it.

## 3. Copied installation (whole app + DB to a new machine)

Same mechanism and same limitation as §2 — a full installation copy *is* a copied database from this system's point of view. Nothing about copying the executable itself changes the analysis; the executable contains no secret that differs machine-to-machine (same as `monthly_password.rs`'s `SK_FRAGMENTS`, which are also compiled into every copy of the binary identically).

## 4. Machine cloning (VM snapshot/restore, disk image duplication)

**Attack:** clone a licensed machine's entire disk (or VM snapshot) to run two simultaneous "identical" instances.

**Protection implemented:** both clones share the same `device_id` *and* the same `machine_fingerprint` at the moment of cloning (fingerprint inputs — computer name, username, processor identifier, see below — are typically preserved by a disk/VM clone). This is **the one scenario the fingerprint genuinely cannot distinguish**, stated plainly: a bit-for-bit clone looks identical to the original by every signal this design collects. The only realistic detection is server-side and probabilistic — `license_validation_logs` recording two `/validate-license` (or `/heartbeat`) calls for the same `device_id` from materially different network locations in a short window is an anomaly-detection concern for the admin dashboard (`LICENSE_SYSTEM_DESIGN.md`'s admin dashboard design lists Audit Logs for exactly this), not something `src/license/` on either clone can know by itself.

## 5. Machine fingerprint — exact inputs and their honest strength

`src/license/fingerprint.rs` hashes (SHA-256): `COMPUTERNAME` + `USERNAME` + `PROCESSOR_IDENTIFIER` (all read via `std::env::var`, no new OS-level dependency added). This is a **weak-to-moderate** fingerprint, by design and by necessity:

- **Why not stronger (e.g. disk volume serial, motherboard UUID via WMI):** would require either a new dependency (`wmi`/`winreg`-style crates) or `unsafe` FFI, for a signal that a determined attacker can still spoof (environment variables and WMI-reported hardware IDs are both trivially overridable by anyone with admin rights on their own machine). Given the module's own stated threat model (§ preamble — a licensing gate, not a security boundary protecting the actual sensitive asset), the cost of that additional dependency/complexity was judged not worth the marginal robustness gain. This is the same judgment call `monthly_password.rs` already made for the existing auth gate, applied consistently here.
- **Why not weaker (e.g. no fingerprint at all, `device_id` alone):** `device_id` alone is *generated by the client* — nothing stops a copy of the database from claiming to be "the same device" with zero additional signal. The fingerprint at least detects the common, non-adversarial case (a customer innocently copying their whole user profile to a new PC without realizing this app's data would come along) and provides a machine-derived signal that's independent of anything stored *inside* `local_license` itself.
- **Legitimate drift is expected and must not brick the app:** a computer rename or Windows user-profile migration changes the fingerprint on the *same*, still-legitimately-licensed machine. This is why §5 of `LICENSE_SYSTEM_DESIGN.md` treats a fingerprint mismatch as a **logged signal reported to the server**, never a local hard block — the client-side code in this phase does not reject a license on fingerprint mismatch by itself; only a server-side policy decision (once a server exists) should ever do that, informed by pattern (one mismatch vs. many devices worth of mismatches).

## 6. Fail-open vs. fail-closed — the one rule enforced everywhere in `src/license/`

Every branch in `LicenseStatus`'s derivation (`license::validation`) has an explicit, enumerated outcome — `NotActivated`, `Active`, `ActiveOfflineGrace`, `GracePeriodExpired`, `Suspended`, `Expired`. There is **no catch-all "else → Active"**: a database read error, a parse error on a stored timestamp, a clock read that fails, or any other unexpected condition resolves to the same treatment as `GracePeriodExpired` (fail closed), never silently to `Active`. This is verified by `license::validation`'s own unit tests (malformed-data test cases) and is the single most important property of this module from a security standpoint — an attacker who can induce an *error* (e.g. a corrupted `local_license` row) gains nothing, because errors and "expired" resolve identically.

## 7. What this phase deliberately does NOT protect against

Stated explicitly rather than left implicit:
- **Binary patching / debugger-based bypass** (e.g. patching out the `check_status` call, or the future enforcement switch in `license::should_enforce`) — no client-side anti-tamper/obfuscation is in scope for this phase. Standard limitation of any client-enforced license on software the customer fully controls the execution environment of.
- **Reverse-engineering the fingerprint algorithm** to spoof a specific target fingerprint — the algorithm is open in this very document and in the source; no obscurity is claimed or relied upon (matching `monthly_password.rs`'s own stated posture: "cannot be closed by obfuscating further").
- **Malicious server operators / MITM on the validate-license call** — out of scope until the real server and its TLS/auth story exist; noted here so it isn't forgotten when that phase starts.

None of the above undermines the actual sensitive asset this application holds (client banking data) — that protection is, and remains, the data-at-rest encryption layer (`db/encryption.rs`, SQLCipher), entirely independent of the licensing system. This review's scope is the *licensing* system's own robustness, not a claim that licensing is what protects customer data.
