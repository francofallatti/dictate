//! Local transcription with whisper.cpp through `whisper-rs`.
//! The ggml model is downloaded from Hugging Face on first use.

use super::{clean_transcript, Hint, Transcriber};
use crate::config::{Paths, WhisperConfig};
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const HF_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
const KNOWN_MODELS: &[&str] = &[
    "tiny", "tiny.en", "base", "base.en", "small", "small.en", "medium", "medium.en",
    "large-v1", "large-v2", "large-v3", "large-v3-turbo",
];

pub struct WhisperTranscriber {
    model_path: PathBuf,
    model_url: Option<String>,
    threads: i32,
    fast_short_audio: bool,
    ctx: Mutex<Option<Arc<WhisperContext>>>,
}

/// whisper's encoder always sees a 30 s window (1500 audio-context frames). For a 4 s dictation
/// that is mostly padding; limiting the context to what the clip needs cuts encoder time by a
/// large factor. Keep a generous margin and never go below 512 frames (~10 s).
fn audio_ctx_for(secs: f32) -> i32 {
    let needed = ((secs + 4.0) / 30.0 * 1500.0).ceil() as i32;
    needed.clamp(512, 1500)
}

impl WhisperTranscriber {
    pub fn new(cfg: &WhisperConfig, paths: &Paths) -> Result<Self> {
        let (model_path, model_url) = if !cfg.model_path.trim().is_empty() {
            (PathBuf::from(cfg.model_path.trim()), None)
        } else {
            let name = cfg.model.trim();
            if !KNOWN_MODELS.contains(&name) {
                warn!(model = name, "model name is not in the known list; download may fail");
            }
            let file = format!("ggml-{name}.bin");
            (paths.models_dir.join(&file), Some(format!("{HF_BASE}/{file}")))
        };
        // ggml synchronises its workers with barriers, so one thread landing on a busy or
        // low-power core stalls all of them. Four threads are close to the optimum on hybrid
        // (P/E-core) laptops and degrade gracefully under load; users with big desktops can raise it.
        let threads = if cfg.threads == 0 {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 4) as i32
        } else {
            cfg.threads as i32
        };
        Ok(Self { model_path, model_url, threads, fast_short_audio: cfg.fast_short_audio, ctx: Mutex::new(None) })
    }

    fn ensure_model(&self) -> Result<()> {
        if self.model_path.exists() {
            return Ok(());
        }
        let url = self.model_url.as_ref().ok_or_else(|| {
            anyhow!("whisper model not found at {}", self.model_path.display())
        })?;
        info!(url, dest = %self.model_path.display(), "downloading whisper model (first run only)");
        if let Some(dir) = self.model_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.model_path.with_extension("part");
        let client = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()?;
        let mut resp = client.get(url).send()?.error_for_status().context("model download")?;
        let total = resp.content_length().unwrap_or(0);
        let mut file = std::fs::File::create(&tmp)?;
        let written = resp.copy_to(&mut file)?;
        drop(file);
        if total > 0 && written != total {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("incomplete download: {written} of {total} bytes");
        }
        std::fs::rename(&tmp, &self.model_path)?;
        info!(bytes = written, "model downloaded");
        Ok(())
    }

    fn context(&self) -> Result<Arc<WhisperContext>> {
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            return Ok(Arc::clone(ctx));
        }
        self.ensure_model()?;
        info!(model = %self.model_path.display(), threads = self.threads, "loading whisper model");
        // Route whisper.cpp / ggml stderr chatter through `tracing` (filtered by `whisper_rs=`).
        whisper_rs::install_logging_hooks();
        let started = std::time::Instant::now();
        let params = WhisperContextParameters::default();
        let ctx = WhisperContext::new_with_params(&self.model_path, params)
            .map_err(|e| anyhow!("loading whisper model: {e:?}"))?;
        info!(elapsed_ms = started.elapsed().as_millis() as u64, "whisper model loaded");
        let ctx = Arc::new(ctx);
        *self.ctx.lock().unwrap() = Some(Arc::clone(&ctx));
        Ok(ctx)
    }
}

impl Transcriber for WhisperTranscriber {
    fn name(&self) -> &'static str {
        "whisper.cpp"
    }

    fn warm_up(&self) -> Result<()> {
        self.context().map(|_| ())
    }

    fn transcribe(&self, pcm: &[f32], hint: &Hint) -> Result<String> {
        let ctx = self.context()?;
        // whisper.cpp wants at least ~1 s of audio; pad with silence.
        let min_len = (crate::audio::TARGET_RATE as f32 * 1.1) as usize;
        let mut audio = pcm.to_vec();
        if audio.len() < min_len {
            audio.resize(min_len, 0.0);
        }

        let secs = pcm.len() as f32 / crate::audio::TARGET_RATE as f32;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        let audio_ctx = if self.fast_short_audio { audio_ctx_for(secs) } else { 0 };
        if audio_ctx > 0 && audio_ctx < 1500 {
            params.set_audio_ctx(audio_ctx);
        }
        params.set_translate(false);
        params.set_no_context(true);
        params.set_single_segment(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_token_timestamps(false);
        let lang = hint.language.trim();
        if !lang.is_empty() {
            params.set_language(Some(lang)); // "auto" triggers detection
        }
        if !hint.initial_prompt.trim().is_empty() {
            params.set_initial_prompt(hint.initial_prompt.trim());
        }

        let started = std::time::Instant::now();
        let mut state = ctx.create_state().map_err(|e| anyhow!("creating whisper state: {e:?}"))?;
        state.full(params, &audio).map_err(|e| anyhow!("whisper inference failed: {e:?}"))?;
        let mut text = String::new();
        for segment in state.as_iter() {
            let piece = segment.to_str_lossy().map_err(|e| anyhow!("reading segment: {e:?}"))?;
            text.push_str(&piece);
            text.push(' ');
        }
        let cleaned = clean_transcript(&text);
        info!(
            audio_secs = secs,
            elapsed_ms = started.elapsed().as_millis() as u64,
            audio_ctx,
            threads = self.threads,
            chars = cleaned.len(),
            "whisper transcription done"
        );
        Ok(cleaned)
    }
}
