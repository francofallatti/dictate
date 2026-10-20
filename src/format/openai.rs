//! Clean-up with an OpenAI-compatible chat completions endpoint.

use super::{system_prompt, tidy_llm_output, user_message, FormatContext, Formatter};
use crate::config::{resolve_secret, OpenAiLlmConfig};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{info, warn};

pub struct OpenAiFormatter {
    api_key: String,
    base_url: String,
    model: String,
    system: String,
    client: reqwest::blocking::Client,
}

impl OpenAiFormatter {
    pub fn new(cfg: &OpenAiLlmConfig, extra_instructions: String) -> Result<Self> {
        let api_key = resolve_secret(&cfg.api_key, &cfg.api_key_env).ok_or_else(|| {
            anyhow!("OpenAI formatter selected but no API key: set format.openai.api_key or {}", cfg.api_key_env)
        })?;
        Ok(Self {
            api_key,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            system: system_prompt(&extra_instructions),
            client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(45)).build()?,
        })
    }
}

impl Formatter for OpenAiFormatter {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn format(&self, raw: &str, ctx: &FormatContext) -> Result<String> {
        let body = json!({
            "model": self.model,
            "temperature": 0.2,
            "messages": [
                { "role": "system", "content": self.system },
                { "role": "user", "content": user_message(raw, ctx) }
            ],
        });
        let started = std::time::Instant::now();
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("calling OpenAI chat completions")?;
        let status = resp.status();
        let text = resp.text()?;
        if !status.is_success() {
            anyhow::bail!("OpenAI request failed: HTTP {status}: {}", text.chars().take(400).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text).context("parsing chat completion")?;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        let tidy = tidy_llm_output(content);
        info!(elapsed_ms = started.elapsed().as_millis() as u64, "openai formatting done");
        if tidy.is_empty() {
            warn!("OpenAI returned empty text; pasting the raw transcript");
            return Ok(raw.to_string());
        }
        Ok(tidy)
    }
}
