// text_safety.rs — UTF-8 char-boundary-safe string slicing helpers
// (Phase 4L.2.2 — UTF-8 Crash Hardening).
//
// Rust's `&str` byte-index slicing (`&s[..n]`, `&s[a..b]`) panics if `n`/`a`/
// `b` don't land exactly on a UTF-8 character boundary. Several call sites
// across this crate compute a byte offset from one source (a fixed constant,
// a differently-normalized copy of the string, a Unicode-aware regex match)
// and then slice a *different* string — usually real bank narration, OCR, or
// external-provider text — at that offset, which is only safe by accident
// when the text happens to be pure ASCII. Real customer/OCR/provider text
// isn't guaranteed to be (₹, accented names, Unicode digit variants, ...),
// so those sites can panic on legitimate production input.
//
// These two helpers turn an arbitrary, untrusted byte offset into one that's
// always safe to slice at — never panicking, at the cost of rounding down to
// the nearest valid character boundary when the requested offset lands
// mid-character. That's the right trade-off for the call sites that use
// this (best-effort log-message truncation, best-effort text-region
// extraction) — losing at most 3 bytes of a truncation point is harmless;
// crashing the whole import/classification pass is not.

/// Returns the largest byte index `<= idx` (and `<= s.len()`) that is a
/// valid UTF-8 character boundary in `s`. A no-op for any `idx` that's
/// already a boundary — in particular, always a no-op for pure-ASCII `s`,
/// since every byte index is a boundary there.
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Returns the longest prefix of `s` that is no more than `max_bytes` bytes
/// long and ends on a valid character boundary — safe to use in place of
/// `&s[..s.len().min(max_bytes)]`, which panics if that byte offset lands
/// mid-character.
pub fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    &s[..floor_char_boundary(s, max_bytes)]
}

/// Case-insensitive (ASCII-only) substring search that returns a byte
/// offset valid *in `s` itself* — unlike the common `s.to_lowercase().find(pat)`
/// idiom, whose returned offset is only guaranteed to exist in the
/// *lowercased copy*, not in `s`. `.to_lowercase()` can change a
/// character's byte length for some Unicode inputs (e.g. `İ` U+0130, 2
/// bytes, lowercases to `i̇`, 3 bytes), which can shift everything after
/// it out of alignment — a byte offset found this way and then used to
/// slice `s` directly is a genuine (if narrow) source of silently wrong
/// — not just unsafe — text extraction, not only the panic
/// `floor_char_boundary` alone would guard against. Only case-folds
/// ASCII bytes (`eq_ignore_ascii_case`), which is sufficient for every
/// current caller's patterns (all plain ASCII).
pub fn find_ascii_ci(s: &str, pat: &str) -> Option<usize> {
    let sb = s.as_bytes();
    let pb = pat.as_bytes();
    if pb.is_empty() || sb.len() < pb.len() {
        return None;
    }
    (0..=sb.len() - pb.len())
        .find(|&i| s.is_char_boundary(i) && sb[i..i + pb.len()].eq_ignore_ascii_case(pb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_char_boundary_is_a_no_op_for_ascii() {
        assert_eq!(floor_char_boundary("hello world", 5), 5);
        assert_eq!(floor_char_boundary("hello world", 0), 0);
        assert_eq!(floor_char_boundary("hello world", 11), 11);
    }

    #[test]
    fn floor_char_boundary_clamps_an_out_of_range_index_to_the_string_length() {
        assert_eq!(floor_char_boundary("hi", 999), 2);
        assert_eq!(floor_char_boundary("", 5), 0);
    }

    #[test]
    fn floor_char_boundary_rounds_down_out_of_a_two_byte_character() {
        // "₹" is U+20B9, 3 bytes (0xE2 0x82 0xB9). "a₹b" = 'a'(1) + ₹(3) + 'b'(1).
        let s = "a₹b";
        assert_eq!(s.len(), 5);
        // Byte 2 and 3 are mid-₹ — must round down to 1 (right after 'a').
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 3), 1);
        // Byte 4 is the boundary right after ₹ — already valid, no rounding.
        assert_eq!(floor_char_boundary(s, 4), 4);
    }

    #[test]
    fn floor_char_boundary_rounds_down_out_of_a_four_byte_emoji() {
        // "😀" (U+1F600) is 4 bytes.
        let s = "x😀y";
        assert_eq!(s.len(), 6);
        for mid in 2..=4 {
            assert_eq!(
                floor_char_boundary(s, mid),
                1,
                "byte {mid} should round down to 1"
            );
        }
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn safe_prefix_matches_naive_slicing_for_ascii() {
        assert_eq!(safe_prefix("hello world", 5), "hello");
        assert_eq!(safe_prefix("short", 100), "short");
        assert_eq!(safe_prefix("", 10), "");
        assert_eq!(safe_prefix("anything", 0), "");
    }

    #[test]
    fn safe_prefix_never_panics_and_never_splits_a_multibyte_character() {
        // The exact bug class this hardens: `&s[..s.len().min(N)]` on real
        // narration/provider text panics when N lands inside a multi-byte
        // character. safe_prefix must return a valid, complete-characters-
        // only string instead, for every possible cut point.
        let samples = [
            "Reliance Jio ₹499 Recharge",
            "मुंबई शाखा - ACCOUNT TRANSFER", // Devanagari (Mumbai branch)
            "café Münchën 😀 payment",
            "पैसे",
        ];
        for s in samples {
            for n in 0..=s.len() + 2 {
                let out = safe_prefix(s, n);
                assert!(out.len() <= n.min(s.len()));
                // The result must itself be a valid, complete &str slice —
                // this line would panic (not just assert-fail) if
                // safe_prefix ever returned a mid-character cut.
                assert!(s.starts_with(out));
            }
        }
    }

    #[test]
    fn safe_prefix_truncates_a_realistic_ai_provider_error_body() {
        // Mirrors ai_classifier.rs's `&text[..text.len().min(200)]` call
        // site: an arbitrary, non-ASCII HTTP error body from an external
        // provider, truncated for a log/error message.
        let body = "エラー: リクエストが無効です。".repeat(20); // Japanese, all multi-byte
        let truncated = safe_prefix(&body, 200);
        assert!(truncated.len() <= 200);
        assert!(body.starts_with(truncated));
    }

    #[test]
    fn find_ascii_ci_matches_case_insensitively() {
        assert_eq!(find_ascii_ci("Invoice PAGE 2 of 5", "page "), Some(8));
        assert_eq!(find_ascii_ci("invoice page 2 of 5", "PAGE "), Some(8));
        assert_eq!(find_ascii_ci("no marker here", "page "), None);
    }

    #[test]
    fn find_ascii_ci_returns_a_position_valid_in_the_original_string_even_past_a_length_changing_lowercase_char(
    ) {
        // 'İ' (U+0130, 2 bytes) lowercases to "i̇" (3 bytes) — the classic
        // case where `s.to_lowercase().find(pat)` returns an offset that
        // no longer corresponds to the same position in `s`.
        let s = "\u{0130}Cafe Page 2";
        let pos = find_ascii_ci(s, "page ").expect("should find \"Page \"");
        assert_eq!(
            &s[pos..pos + 5],
            "Page ",
            "the returned offset must point at the real match in s"
        );
    }

    #[test]
    fn find_ascii_ci_never_panics_on_multibyte_haystacks_with_no_match() {
        let samples = ["पेमेंट भुगतान", "café münchën", "₹₹₹ 😀 ₹₹₹"];
        for s in samples {
            for pat in ["page ", "l prop", "propr"] {
                assert_eq!(find_ascii_ci(s, pat), None); // must not panic
            }
        }
    }
}
