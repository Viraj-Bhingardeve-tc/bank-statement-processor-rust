# License API Specification — Phase 3A

**Status: specification only. No server implements this yet.** This is the contract `src/license/client.rs`'s `LicenseApiClient` trait is designed against, and what a future backend must implement for the desktop app's `HttpLicenseClient` (not yet written — see `LICENSE_SYSTEM_DESIGN.md` §9) to work against without a client-side change.

All endpoints: `Content-Type: application/json`. All responses use a consistent envelope:

```json
{ "ok": true,  "data": { ... } }
{ "ok": false, "error": { "code": "LICENSE_EXPIRED", "message": "..." } }
```

All timestamps ISO-8601 UTC. All monetary amounts in minor units (paise), never floats.

Auth: every endpoint except `POST /login` requires `Authorization: Bearer <session_token>` (returned by `/login`). This is the *server account* session (matching the `users` table), unrelated to and not replacing the desktop's existing monthly-password screen (`LICENSE_SYSTEM_DESIGN.md` §1).

---

## `POST /login`

Authenticate a customer account (the *server* account, i.e. `users` table — not the desktop's `auth::validate_credentials`).

**Request**
```json
{ "email": "customer@example.com", "password": "..." }
```

**Response `200`**
```json
{ "ok": true, "data": {
    "session_token": "opaque-bearer-token",
    "user_id": "usr_123",
    "expires_at": "2026-08-09T00:00:00Z"
} }
```

**Errors:** `401 INVALID_CREDENTIALS`.

---

## `POST /activate-license`

Binds a license key to this device. Called once per (license, device) pair — subsequent app launches use `/validate-license`, not this.

**Request**
```json
{
  "license_key": "XXXX-XXXX-XXXX-XXXX",
  "device_id": "a1b2c3d4-...-uuid",
  "machine_fingerprint": "sha256-hex",
  "device_label": "DESKTOP-AB12CD"
}
```

**Response `200`**
```json
{ "ok": true, "data": {
    "license_id": "lic_456",
    "customer_id": "cus_789",
    "subscription_type": "yearly",
    "status": "active",
    "expires_at": "2027-07-09T00:00:00Z",
    "grace_period_days": 7
} }
```

**Errors:**
- `404 LICENSE_NOT_FOUND` — unknown key.
- `409 DEVICE_LIMIT_REACHED` — `licenses.max_devices` already met by other active devices; response includes the existing device list so the customer/admin can deactivate one (see `POST /devices/{id}/deactivate` in the admin surface, not a customer-facing endpoint — out of scope for this list of 7, noted in `LICENSE_SYSTEM_DESIGN.md`'s admin dashboard design).
- `410 LICENSE_REVOKED` / `410 LICENSE_EXPIRED`.

---

## `POST /validate-license`

Called on every startup when online (`LICENSE_SYSTEM_DESIGN.md` §4). Cheap, idempotent, safe to call frequently.

**Request**
```json
{
  "license_id": "lic_456",
  "device_id": "a1b2c3d4-...-uuid",
  "machine_fingerprint": "sha256-hex",
  "client_clock": "2026-07-09T10:15:00Z"
}
```

`client_clock` — the desktop's own idea of "now," sent explicitly so the server can log/flag large disagreement with its own clock (a signal for clock-rollback abuse across the fleet; the desktop's own local clock-rollback defense is independent of this and does not depend on the server seeing it — see `LICENSE_SECURITY_REVIEW.md` §1).

**Response `200`**
```json
{ "ok": true, "data": {
    "status": "active",
    "expires_at": "2027-07-09T00:00:00Z",
    "grace_period_days": 7,
    "server_time": "2026-07-09T10:15:03Z",
    "fingerprint_matched": true
} }
```

`status` ∈ `active | expired | suspended | revoked | device_mismatch`. `fingerprint_matched: false` is informational, not itself a rejection — see `LICENSE_SYSTEM_DESIGN.md` §5 on why a fingerprint drift is logged, not auto-blocked.

**Errors:** `404 DEVICE_NOT_ACTIVATED` (this device_id was never activated against this license — client should fall back to `/activate-license` or prompt re-activation, not silently retry validate).

---

## `POST /refresh-license`

Re-fetches license terms after a plan change (renewal, upgrade, admin edit) without a full re-activation. Same request/response shape as `/validate-license`; kept as a distinct endpoint (not just relying on the next `/validate-license`) so the desktop can call it *immediately* after the user completes a renewal/payment flow, to reflect the new `expires_at` without waiting for the next natural validation cycle.

**Request** — identical to `/validate-license`.
**Response** — identical to `/validate-license`.

---

## `POST /logout`

Invalidates the current server session token (the `/login` bearer token). Does **not** deactivate the device or affect `local_license` — logging out of the server account and the app continuing to run on a still-valid, already-activated license are independent (the device stays activated; only the account session ends).

**Request:** empty body, `Authorization` header only.
**Response `200`:** `{ "ok": true, "data": {} }`.

---

## `GET /subscription`

Fetches the current subscription/billing summary for the logged-in account — for an in-app "Manage Subscription" screen (not built this phase, but the endpoint is specified now so the UI can be added later without an API change).

**Response `200`**
```json
{ "ok": true, "data": {
    "subscription_id": "sub_321",
    "plan_type": "yearly",
    "status": "active",
    "current_period_end": "2027-07-09T00:00:00Z",
    "auto_renew": true,
    "licenses": [
      { "license_id": "lic_456", "status": "active", "devices_active": 1, "max_devices": 1 }
    ]
} }
```

---

## `POST /heartbeat`

Lightweight liveness ping, separate from `/validate-license`, intended to be called periodically *while the app is running* (not just at startup) — e.g. every few hours — so a license revoked mid-session (support-desk action, chargeback) is noticed sooner than the next app restart, without the cost of a full validation payload.

**Request**
```json
{ "license_id": "lic_456", "device_id": "a1b2c3d4-...-uuid" }
```

**Response `200`**
```json
{ "ok": true, "data": { "status": "active" } }
```

Same `status` enum as `/validate-license`. A non-`active` status here should trigger the same client-side handling as a failed `/validate-license` (re-derive `LicenseStatus`, do not treat a heartbeat failure as merely "log and ignore").

---

## Error codes (all endpoints)

| Code | HTTP | Meaning |
|---|---|---|
| `INVALID_CREDENTIALS` | 401 | `/login` failure |
| `UNAUTHORIZED` | 401 | missing/expired bearer token |
| `LICENSE_NOT_FOUND` | 404 | unknown license key |
| `DEVICE_NOT_ACTIVATED` | 404 | device never activated against this license |
| `DEVICE_LIMIT_REACHED` | 409 | activation would exceed `max_devices` |
| `LICENSE_EXPIRED` | 410 | subscription period ended |
| `LICENSE_REVOKED` | 410 | manually revoked (admin action) |
| `LICENSE_SUSPENDED` | 423 | temporarily suspended (e.g. failed payment), distinct from revoked — recoverable without re-activation |
| `RATE_LIMITED` | 429 | too many validation calls in a short window |
| `SERVER_ERROR` | 500 | — |

The desktop client (`src/license/client.rs`) models this as `enum ApiError` — see `LICENSE_SYSTEM_DESIGN.md` §9 for how the trait/error type is structured to keep today's `OfflineClient` and tomorrow's `HttpLicenseClient` interchangeable.
