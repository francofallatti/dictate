//! Clean-up with the Claude Messages API (raw HTTP, no SDK for Rust).

use super::{system_prompt, tidy_llm_output, user_message, FormatContext, Formatter};
use crate::config::{resolve_secret, ClaudeConfig};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{info, warn};

pub struct ClaudeFormatter {
    api_key: String,
    base_url: String,
    model: String,
    effort: String,
    system: String,
    client: reqwest::blocking::Client,
}

impl ClaudeFormatter {
    pub fn new(cfg: &ClaudeConfig, extra_instructions: String) -> Result<Self> {
        let api_key = resolve_secret(&cfg.api_key, &cfg.api_key_env).ok_or_else(|| {
            anyhow!("Claude formatter selected but no API key: set format.claude.api_key or {}", cfg.api_key_env)
        })?;
        let effort = match cfg.effort.trim() {
            "" => "low".to_string(),
            e @ ("low" | "medium" | "high") => e.to_string(),
            other => anyhow::bail!("format.claude.effort must be low, medium or high (got {other:?})"),
        };
        Ok(Self {
            api_key,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            effort,
            system: system_prompt(&extra_instructions),
            client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(45)).build()?,
        })
    }
}

impl Formatter for ClaudeFormatter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn format(&self, raw: &str, ctx: &FormatContext) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": self.system,
            "messages": [{ "role": "user", "content": user_message(raw, ctx) }],
            "output_config": { "effort": self.effort },
        });
        let started = std::time::Instant::now();
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("calling Claude")?;
        let status = resp.status();
        let text = resp.text()?;
        if !status.is_success() {
            anyhow::bail!("Claude request failed: HTTP {status}: {}", text.chars().take(400).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text).context("parsing Claude response")?;
        let stop_reason = v["stop_reason"].as_str().unwrap_or("");
        if stop_reason == "refusal" {
            warn!("Claude declined to process this transcript; pasting the raw text");
            return Ok(raw.to_string());
        }
        let mut out = String::new();
        if let Some(blocks) = v["content"].as_array() {
            for b in blocks {
                if b["type"] == "text" {
                    if let Some(t) = b["text"].as_str() {
                        out.push_str(t);
                    }
                }
            }
        }
        if stop_reason == "max_tokens" {
            warn!("Claude output hit max_tokens; result may be truncated");
        }
        let tidy = tidy_llm_output(&out);
        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            input_tokens = v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens = v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            "claude formatting done"
        );
        if tidy.is_empty() {
            warn!("Claude returned empty text; pasting the raw transcript");
            return Ok(raw.to_string());
        }
        Ok(tidy)
    }
}
