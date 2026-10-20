//! Cloud transcription through the OpenAI audio transcription endpoint
//! (`POST /v1/audio/transcriptions`, multipart). Works with any compatible server
//! by changing `base_url`.

use super::{clean_transcript, wav_bytes, Hint, Transcriber};
use crate::config::{resolve_secret, OpenAiSttConfig};
use anyhow::{anyhow, Context, Result};
use reqwest::blocking::multipart;
use std::time::Duration;
use tracing::info;

pub struct OpenAiTranscriber {
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl OpenAiTranscriber {
    pub fn new(cfg: &OpenAiSttConfig) -> Result<Self> {
        let api_key = resolve_secret(&cfg.api_key, &cfg.api_key_env).ok_or_else(|| {
            anyhow!("OpenAI STT selected but no API key: set stt.openai.api_key or {}", cfg.api_key_env)
        })?;
        Ok(Self {
            api_key,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build()?,
        })
    }
}

#[derive(serde::Deserialize)]
struct TranscriptionResponse {
    text: String,
}

impl Transcriber for OpenAiTranscriber {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn transcribe(&self, pcm: &[f32], hint: &Hint) -> Result<String> {
        let wav = wav_bytes(pcm, crate::audio::TARGET_RATE)?;
        let mut form = multipart::Form::new()
            .text("model", self.model.clone())
            .text("response_format", "json")
            .part("file", multipart::Part::bytes(wav).file_name("audio.wav").mime_str("audio/wav")?);
        let lang = hint.language.trim();
        if !lang.is_empty() && lang != "auto" {
            form = form.text("language", lang.to_string());
        }
        if !hint.initial_prompt.trim().is_empty() {
            form = form.text("prompt", hint.initial_prompt.trim().to_string());
        }
        let started = std::time::Instant::now();
        let resp = self
            .client
            .post(format!("{}/v1/audio/transcriptions", self.base_url))
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .context("sending audio to OpenAI")?;
        let status = resp.status();
        let body = resp.text()?;
        if !status.is_success() {
            anyhow::bail!("OpenAI transcription failed: HTTP {status}: {}", body.chars().take(400).collect::<String>());
        }
        let parsed: TranscriptionResponse = serde_json::from_str(&body).context("parsing transcription response")?;
        info!(elapsed_ms = started.elapsed().as_millis() as u64, "openai transcription done");
        Ok(clean_transcript(&parsed.text))
    }
}
