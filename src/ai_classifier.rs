// ai_classifier.rs — AI-powered batch classification using OpenAI / Claude / Gemini.
// Only compiled when the "ai" feature is active.

#![cfg(feature = "ai")]

use anyhow::{anyhow, Context, Result};
use crate::parser::Transaction;

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
    pub idx:         usize,
    pub vendor:      String,
    pub account_head: String,
    pub txn_type:    String,
    pub confidence:  f64,
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

/// Classify a batch of transactions using AI.
/// `progress_cb` is called with (done, total) after each batch completes.
pub fn classify_with_ai<F>(
    txns: &mut Vec<Transaction>,
    provider: AiProvider,
    api_key: &str,
    scope: AiScope,
    mut progress_cb: F,
) -> Result<usize>
where
    F: FnMut(usize, usize),
{
    if api_key.trim().is_empty() {
        return Err(anyhow!("AI API key is empty"));
    }

    let indices: Vec<usize> = txns.iter().enumerate()
        .filter(|(_, t)| {
            if t.is_opening_balance { return false; }
            if matches!(t.status, crate::parser::TransactionStatus::Manual)
                || matches!(t.status, crate::parser::TransactionStatus::Suspense) {
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
        })
        .map(|(i, _)| i)
        .collect();

    let total = indices.len();
    if total == 0 { return Ok(0); }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build http client")?;

    let mut classified = 0usize;

    for chunk in indices.chunks(BATCH_SIZE) {
        let batch: Vec<(usize, &str)> = chunk.iter()
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
                        if !r.vendor.is_empty()       { t.vendor       = r.vendor.clone(); }
                        if !r.account_head.is_empty() { t.account_head = r.account_head.clone(); }
                        if !r.txn_type.is_empty() {
                            t.txn_type = match r.txn_type.as_str() {
                                "Payment"  => crate::parser::VoucherType::Payment,
                                "Receipt"  => crate::parser::VoucherType::Receipt,
                                "Contra"   => crate::parser::VoucherType::Contra,
                                _          => t.txn_type.clone(),
                            };
                        }
                        t.confidence   = r.confidence.clamp(0.0, 1.0);
                        t.status       = crate::parser::TransactionStatus::Classified;
                        t.classification_source = "ai".to_string();
                        if !t.tags.iter().any(|g| g == "ai") { t.tags.push("ai".to_string()); }
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
    let lines: Vec<String> = batch.iter()
        .enumerate()
        .map(|(i, (_, narr))| format!("{}: {}", i + 1, narr))
        .collect();
    format!("Classify these {} transactions:\n{}", batch.len(), lines.join("\n"))
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
        return Err(anyhow!("OpenAI HTTP {}: {}", status, &text[..text.len().min(200)]));
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("parse openai json")?;
    let content = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("[]");

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
        return Err(anyhow!("Claude HTTP {}: {}", status, &text[..text.len().min(200)]));
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("parse claude json")?;
    let content = json["content"][0]["text"]
        .as_str().unwrap_or("[]");

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
        return Err(anyhow!("Gemini HTTP {}: {}", status, &text[..text.len().min(200)]));
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("parse gemini json")?;
    let content = json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str().unwrap_or("[]");

    parse_ai_response(batch, content)
}

// ── Response parser ───────────────────────────────────────────────────────────

fn parse_ai_response(batch: &[(usize, &str)], content: &str) -> Result<Vec<AiClassifyResult>> {
    // Strip markdown code fences if present
    let stripped = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let arr: serde_json::Value = serde_json::from_str(stripped)
        .context("parse AI JSON response")?;

    let items = arr.as_array().ok_or_else(|| anyhow!("AI response is not an array"))?;

    let mut results = Vec::new();
    for (pos, item) in items.iter().enumerate() {
        if pos >= batch.len() { break; }
        let orig_idx = batch[pos].0;
        results.push(AiClassifyResult {
            idx:          orig_idx,
            vendor:       item["vendor"].as_str().unwrap_or("").to_string(),
            account_head: item["account_head"].as_str().unwrap_or("").to_string(),
            txn_type:     item["txn_type"].as_str().unwrap_or("").to_string(),
            confidence:   item["confidence"].as_f64().unwrap_or(0.5),
        });
    }

    Ok(results)
}
