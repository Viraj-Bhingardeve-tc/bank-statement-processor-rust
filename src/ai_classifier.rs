// ai_classifier.rs — AI-powered batch classification using OpenAI / Claude / Gemini.
// Only compiled when the "ai" feature is active (gated by lib.rs's
// `#[cfg(feature = "ai")] pub mod ai_classifier;`, not repeated here).

use crate::parser::Transaction;
use crate::text_safety::safe_prefix;
use anyhow::{anyhow, Context, Result};

const BATCH_SIZE: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiProvider {
    OpenAi,
    Claude,
    Gemini,
}

impl AiProvider {
    pub fn from_idx(idx: i32) -> Self {
        match idx {
            1 => AiProvider::Claude,
            2 => AiProvider::Gemini,
            _ => AiProvider::OpenAi,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiClassifyResult {
    pub idx: usize,
    pub vendor: String,
    pub account_head: String,
    pub txn_type: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiScope {
    /// Only unreviewed / low-confidence (<0.6) transactions.
    Unclassified,
    /// All transactions except opening-balance, suspense and manually-confirmed rows.
    All,
}

impl AiScope {
    pub fn from_idx(idx: i32) -> Self {
        match idx {
            1 => AiScope::All,
            _ => AiScope::Unclassified,
        }
    }
}

/// Is this transaction eligible for AI classification under `scope`?
/// Opening-balance, Manual, and Suspense rows are never eligible regardless
/// of scope. Extracted as its own function (rather than left as an inline
/// closure) so scope selection — which of a batch's transactions would
/// actually be sent — is directly unit-testable without needing a live
/// network call.
fn is_eligible_for_ai(t: &Transaction, scope: AiScope) -> bool {
    if t.is_opening_balance {
        return false;
    }
    if matches!(t.status, crate::parser::TransactionStatus::Manual)
        || matches!(t.status, crate::parser::TransactionStatus::Suspense)
    {
        return false;
    }
    match scope {
        AiScope::Unclassified => {
            matches!(t.status, crate::parser::TransactionStatus::Unreviewed)
                || matches!(t.status, crate::parser::TransactionStatus::NeedsReview)
                || t.confidence < 0.6
        }
        AiScope::All => true,
    }
}

/// Classify a batch of transactions using AI.
/// `progress_cb` is called with (done, total) after each batch completes.
/// `cancel_flag` is checked before each batch — matching old app's functional
/// Cancel button (app.js:3696-3713), which aborts the loop rather than merely
/// hiding the overlay. Already-classified transactions from completed batches
/// are kept; only remaining batches are skipped.
pub fn classify_with_ai<F>(
    txns: &mut [Transaction],
    provider: AiProvider,
    api_key: &str,
    scope: AiScope,
    cancel_flag: &std::sync::atomic::AtomicBool,
    mut progress_cb: F,
) -> Result<usize>
where
    F: FnMut(usize, usize),
{
    if api_key.trim().is_empty() {
        return Err(anyhow!("AI API key is empty"));
    }

    let indices: Vec<usize> = txns
        .iter()
        .enumerate()
        .filter(|(_, t)| is_eligible_for_ai(t, scope))
        .map(|(i, _)| i)
        .collect();

    let total = indices.len();
    if total == 0 {
        return Ok(0);
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build http client")?;

    let mut classified = 0usize;

    for chunk in indices.chunks(BATCH_SIZE) {
        if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
            log::info!(
                "[AI] cancelled by user after {} of {} classified",
                classified,
                total
            );
            break;
        }
        let batch: Vec<(usize, &str)> = chunk
            .iter()
            .map(|&i| (i, txns[i].narration.as_str()))
            .collect();

        let results = match provider {
            AiProvider::OpenAi => call_openai(&client, api_key, &batch),
            AiProvider::Claude => call_claude(&client, api_key, &batch),
            AiProvider::Gemini => call_gemini(&client, api_key, &batch),
        };

        match results {
            Ok(responses) => {
                for r in &responses {
                    if r.idx < txns.len() {
                        let t = &mut txns[r.idx];
                        if !r.vendor.is_empty() {
                            t.vendor = r.vendor.clone();
                        }
                        if !r.account_head.is_empty() {
                            t.account_head = r.account_head.clone();
                        }
                        if !r.txn_type.is_empty() {
                            t.txn_type = match r.txn_type.as_str() {
                                "Payment" => crate::parser::VoucherType::Payment,
                                "Receipt" => crate::parser::VoucherType::Receipt,
                                "Contra" => crate::parser::VoucherType::Contra,
                                _ => t.txn_type.clone(),
                            };
                        }
                        t.confidence = r.confidence.clamp(0.0, 1.0);
                        t.status = crate::parser::TransactionStatus::Classified;
                        t.classification_source = "ai".to_string();
                        if !t.tags.iter().any(|g| g == "ai") {
                            t.tags.push("ai".to_string());
                        }
                        classified += 1;
                    }
                }
            }
            Err(e) => {
                log::warn!("[AI] batch failed: {}", e);
            }
        }

        progress_cb(classified.min(total), total);
    }

    Ok(classified)
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn system_prompt() -> &'static str {
    "You are a financial transaction classifier for Indian businesses. \
Given a list of bank transaction narrations, classify each one. \
Respond with a JSON array, one object per transaction, with fields: \
\"vendor\" (party name, string), \"account_head\" (Tally ledger name, string), \
\"txn_type\" (\"Payment\" | \"Receipt\" | \"Contra\"), \"confidence\" (0.0–1.0). \
Use standard Indian accounting ledger names (e.g. \"Salary\", \"Rent\", \"Bank Charges\", \
\"Sundry Debtors\", \"Sundry Creditors\", \"GST Payable\"). \
Return ONLY the JSON array, no markdown."
}

fn build_user_prompt(batch: &[(usize, &str)]) -> String {
    let lines: Vec<String> = batch
        .iter()
        .enumerate()
        .map(|(i, (_, narr))| format!("{}: {}", i + 1, narr))
        .collect();
    format!(
        "Classify these {} transactions:\n{}",
        batch.len(),
        lines.join("\n")
    )
}

// ── OpenAI ────────────────────────────────────────────────────────────────────

fn call_openai(
    client: &reqwest::blocking::Client,
    api_key: &str,
    batch: &[(usize, &str)],
) -> Result<Vec<AiClassifyResult>> {
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": system_prompt()},
            {"role": "user",   "content": build_user_prompt(batch)}
        ],
        "temperature": 0.2,
        "max_tokens": 1500
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .context("openai request")?;

    let status = resp.status();
    let text = resp.text().context("openai response text")?;
    if !status.is_success() {
        return Err(anyhow!(
            "OpenAI HTTP {}: {}",
            status,
            safe_prefix(&text, 200)
        ));
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("parse openai json")?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("[]");

    parse_ai_response(batch, content)
}

// ── Anthropic Claude ──────────────────────────────────────────────────────────

fn call_claude(
    client: &reqwest::blocking::Client,
    api_key: &str,
    batch: &[(usize, &str)],
) -> Result<Vec<AiClassifyResult>> {
    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 1500,
        "system": system_prompt(),
        "messages": [
            {"role": "user", "content": build_user_prompt(batch)}
        ]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .context("claude request")?;

    let status = resp.status();
    let text = resp.text().context("claude response text")?;
    if !status.is_success() {
        return Err(anyhow!(
            "Claude HTTP {}: {}",
            status,
            safe_prefix(&text, 200)
        ));
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("parse claude json")?;
    let content = json["content"][0]["text"].as_str().unwrap_or("[]");

    parse_ai_response(batch, content)
}

// ── Google Gemini ─────────────────────────────────────────────────────────────

fn call_gemini(
    client: &reqwest::blocking::Client,
    api_key: &str,
    batch: &[(usize, &str)],
) -> Result<Vec<AiClassifyResult>> {
    let prompt = format!("{}\n\n{}", system_prompt(), build_user_prompt(batch));
    let body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"temperature": 0.2, "maxOutputTokens": 1500}
    });

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:generateContent?key={}",
        api_key
    );

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .context("gemini request")?;

    let status = resp.status();
    let text = resp.text().context("gemini response text")?;
    if !status.is_success() {
        return Err(anyhow!(
            "Gemini HTTP {}: {}",
            status,
            safe_prefix(&text, 200)
        ));
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("parse gemini json")?;
    let content = json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("[]");

    parse_ai_response(batch, content)
}

// ── Response parser ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Transaction, TransactionStatus};
    use std::sync::atomic::AtomicBool;

    // ── Requirement #6 (AI confidentiality warning / consent) ────────────────
    // The consent dialog is Slint UI state, not reachable from `cargo test
    // --lib` — but its guarantee ("no data leaves the machine without
    // explicit accept") bottoms out in `classify_with_ai` refusing to make
    // any network call at all in exactly the situations a declined/aborted
    // consent flow produces: a blank API key, nothing in scope, or a
    // cancel flag already set. These tests exercise all three without ever
    // touching the network, so they're safe to run offline/in CI.

    #[test]
    fn empty_api_key_is_rejected_before_any_network_call() {
        let mut txns = vec![Transaction {
            narration: "UPI/DR/123/AIRTEL POSTPAID".to_string(),
            debit: Some(499.0),
            ..Transaction::new("t1")
        }];
        let before = txns[0].clone();
        let cancel = AtomicBool::new(false);

        let result = classify_with_ai(
            &mut txns,
            AiProvider::OpenAi,
            "", // no key — equivalent to a declined/unconfigured request
            AiScope::All,
            &cancel,
            |_, _| {},
        );

        assert!(result.is_err(), "a blank API key must never proceed");
        assert_eq!(
            txns[0].narration, before.narration,
            "transaction must be untouched when the request never happens"
        );
        assert_eq!(txns[0].vendor, before.vendor);
        assert_eq!(txns[0].status, before.status);
    }

    #[test]
    fn whitespace_only_api_key_is_also_rejected() {
        let mut txns = vec![Transaction {
            debit: Some(100.0),
            ..Transaction::new("t1")
        }];
        let cancel = AtomicBool::new(false);
        let result = classify_with_ai(
            &mut txns,
            AiProvider::OpenAi,
            "   ",
            AiScope::All,
            &cancel,
            |_, _| {},
        );
        assert!(result.is_err());
    }

    #[test]
    fn nothing_in_scope_returns_zero_without_a_network_call() {
        // Opening-balance, Suspense, and Manual rows are never eligible for
        // AI classification regardless of scope — with only those present,
        // there is nothing to send, so the function must short-circuit
        // before ever building an HTTP request.
        let mut txns = vec![
            Transaction {
                is_opening_balance: true,
                ..Transaction::new("ob")
            },
            Transaction {
                status: TransactionStatus::Suspense,
                narration: "should never be sent".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                status: TransactionStatus::Manual,
                narration: "should never be sent either".to_string(),
                ..Transaction::new("t2")
            },
        ];
        let cancel = AtomicBool::new(false);
        let result = classify_with_ai(
            &mut txns,
            AiProvider::OpenAi,
            "sk-test-key-not-real",
            AiScope::All,
            &cancel,
            |_, _| {},
        );
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn a_cancel_flag_set_before_the_call_sends_nothing() {
        // Mirrors what happens if the user backs out immediately after the
        // run starts (on_do_ai_cancel sets this same flag from the UI
        // thread): the very first thing the batch loop does is check it,
        // before any provider is contacted.
        let mut txns = vec![Transaction {
            narration: "NEFT/AB1234/RAMESH KUMAR".to_string(),
            credit: Some(5000.0),
            ..Transaction::new("t1")
        }];
        let before_narration = txns[0].narration.clone();
        let cancel = AtomicBool::new(true); // already cancelled

        let result = classify_with_ai(
            &mut txns,
            AiProvider::OpenAi,
            "sk-test-key-not-real",
            AiScope::All,
            &cancel,
            |_, _| {},
        );

        assert_eq!(result.unwrap(), 0, "no batch should be classified");
        assert_eq!(txns[0].narration, before_narration, "row must be untouched");
        assert!(
            txns[0].vendor.is_empty(),
            "no AI vendor suggestion should have been applied"
        );
        assert_eq!(txns[0].classification_source, "");
    }

    // ── is_eligible_for_ai (which rows would actually be sent) ───────────────

    #[test]
    fn unclassified_scope_includes_unreviewed_and_low_confidence_rows() {
        let unreviewed = Transaction::new("t1"); // status defaults to Unreviewed
        let low_conf = Transaction {
            status: TransactionStatus::Classified,
            confidence: 0.3,
            ..Transaction::new("t2")
        };
        assert!(is_eligible_for_ai(&unreviewed, AiScope::Unclassified));
        assert!(is_eligible_for_ai(&low_conf, AiScope::Unclassified));
    }

    #[test]
    fn unclassified_scope_excludes_confidently_classified_rows() {
        let confident = Transaction {
            status: TransactionStatus::Classified,
            confidence: 0.95,
            ..Transaction::new("t1")
        };
        assert!(!is_eligible_for_ai(&confident, AiScope::Unclassified));
    }

    #[test]
    fn all_scope_still_includes_a_confidently_classified_row() {
        let confident = Transaction {
            status: TransactionStatus::Classified,
            confidence: 0.95,
            ..Transaction::new("t1")
        };
        assert!(is_eligible_for_ai(&confident, AiScope::All));
    }

    #[test]
    fn opening_balance_manual_and_suspense_rows_are_never_eligible() {
        let ob = Transaction {
            is_opening_balance: true,
            ..Transaction::new("ob")
        };
        let manual = Transaction {
            status: TransactionStatus::Manual,
            ..Transaction::new("t1")
        };
        let suspense = Transaction {
            status: TransactionStatus::Suspense,
            ..Transaction::new("t2")
        };
        for scope in [AiScope::Unclassified, AiScope::All] {
            assert!(!is_eligible_for_ai(&ob, scope));
            assert!(!is_eligible_for_ai(&manual, scope));
            assert!(!is_eligible_for_ai(&suspense, scope));
        }
    }

    // ── Scope/provider index mapping (pure, worth locking in) ────────────────

    #[test]
    fn provider_from_idx_maps_all_three_providers() {
        assert_eq!(AiProvider::from_idx(0), AiProvider::OpenAi);
        assert_eq!(AiProvider::from_idx(1), AiProvider::Claude);
        assert_eq!(AiProvider::from_idx(2), AiProvider::Gemini);
        // Out-of-range indices fail safe to the default provider rather
        // than panicking or picking an arbitrary one.
        assert_eq!(AiProvider::from_idx(99), AiProvider::OpenAi);
    }

    #[test]
    fn scope_from_idx_maps_both_scopes() {
        assert_eq!(AiScope::from_idx(0), AiScope::Unclassified);
        assert_eq!(AiScope::from_idx(1), AiScope::All);
        assert_eq!(AiScope::from_idx(99), AiScope::Unclassified);
    }
}

fn parse_ai_response(batch: &[(usize, &str)], content: &str) -> Result<Vec<AiClassifyResult>> {
    // Strip markdown code fences if present
    let stripped = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let arr: serde_json::Value =
        serde_json::from_str(stripped).context("parse AI JSON response")?;

    let items = arr
        .as_array()
        .ok_or_else(|| anyhow!("AI response is not an array"))?;

    let mut results = Vec::new();
    for (pos, item) in items.iter().enumerate() {
        if pos >= batch.len() {
            break;
        }
        let orig_idx = batch[pos].0;
        results.push(AiClassifyResult {
            idx: orig_idx,
            vendor: item["vendor"].as_str().unwrap_or("").to_string(),
            account_head: item["account_head"].as_str().unwrap_or("").to_string(),
            txn_type: item["txn_type"].as_str().unwrap_or("").to_string(),
            confidence: item["confidence"].as_f64().unwrap_or(0.5),
        });
    }

    Ok(results)
}
