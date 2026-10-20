//! Configuration: `%APPDATA%\Dictate\config.toml`, created with defaults on first run.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkey: HotkeyConfig,
    pub audio: AudioConfig,
    pub stt: SttConfig,
    pub format: FormatConfig,
    pub paste: PasteConfig,
    pub history: HistoryConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Keys that must be held together to dictate. Names: ctrl, lctrl, rctrl, win, lwin, rwin,
    /// alt, lalt, ralt, shift, lshift, rshift, space, caps, tab, f1..f24, a..z, 0..9, `vk:0x..`.
    pub chord: Vec<String>,
    /// A second press within this window after a quick tap locks hands-free mode.
    /// A quick tap with no second press cancels the dictation.
    pub double_tap_ms: u64,
    /// Press Escape to cancel an active dictation.
    pub escape_cancels: bool,
    /// Hard stop for a single dictation.
    pub max_duration_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Substring of the input device name. Empty = system default.
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// "whisper" (local whisper.cpp) or "openai".
    pub backend: String,
    /// ISO 639-1 code ("es", "en") or "auto".
    pub language: String,
    /// Optional vocabulary hint (names, jargon) passed to the recognizer.
    pub initial_prompt: String,
    pub whisper: WhisperConfig,
    pub openai: OpenAiSttConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WhisperConfig {
    /// ggml model name downloaded from Hugging Face on first use:
    /// tiny, base, small, medium, large-v3-turbo (add `.en` for English-only variants).
    pub model: String,
    /// Explicit path to a ggml .bin; overrides `model` when set.
    pub model_path: String,
    /// 0 = number of available cores.
    pub threads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiSttConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub base_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatConfig {
    /// "none", "claude" or "openai".
    pub backend: String,
    /// Extra instructions appended to the clean-up prompt (tone, signature, glossary...).
    pub extra_instructions: String,
    pub claude: ClaudeConfig,
    pub openai: OpenAiLlmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub base_url: String,
    pub model: String,
    /// low | medium | high
    pub effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiLlmConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub base_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PasteConfig {
    /// Put the previous clipboard content back after pasting.
    pub restore_clipboard: bool,
    /// Time given to the target app to read the clipboard before restoring it.
    pub restore_delay_ms: u64,
    /// Append a trailing space after the pasted text.
    pub trailing_space: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub sounds: bool,
    pub tray: bool,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            chord: vec!["ctrl".into(), "win".into()],
            double_tap_ms: 500,
            escape_cancels: true,
            max_duration_secs: 20 * 60,
        }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            backend: "whisper".into(),
            language: "auto".into(),
            initial_prompt: String::new(),
            whisper: WhisperConfig::default(),
            openai: OpenAiSttConfig::default(),
        }
    }
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self { model: "base".into(), model_path: String::new(), threads: 0 }
    }
}

impl Default for OpenAiSttConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: "https://api.openai.com".into(),
            model: "gpt-4o-mini-transcribe".into(),
        }
    }
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            backend: "none".into(),
            extra_instructions: String::new(),
            claude: ClaudeConfig::default(),
            openai: OpenAiLlmConfig::default(),
        }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-haiku-5-5".into(),
            effort: "low".into(),
        }
    }
}

impl Default for OpenAiLlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: "https://api.openai.com".into(),
            model: "gpt-4o-mini".into(),
        }
    }
}

impl Default for PasteConfig {
    fn default() -> Self {
        Self { restore_clipboard: true, restore_delay_ms: 250, trailing_space: true }
    }
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { sounds: true, tray: true }
    }
}

/// Well-known locations for config, models, history and logs.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
    pub history_file: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "Dictate").context("cannot resolve user directories")?;
        let config_dir = dirs.config_dir().to_path_buf();
        let data_dir = dirs.data_dir().to_path_buf();
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            models_dir: data_dir.join("models"),
            history_file: data_dir.join("history.jsonl"),
            log_dir: data_dir.join("logs"),
            data_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.config_file.parent().unwrap_or(Path::new(".")),
            &self.data_dir,
            &self.models_dir,
            &self.log_dir,
        ] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

impl Config {
    /// Loads the config file, writing a commented default when it does not exist yet.
    /// Returns the config and whether it was freshly created.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool)> {
        if path.exists() {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            Ok((cfg, false))
        } else {
            let cfg = Config::default();
            let text = toml::to_string_pretty(&cfg)?;
            std::fs::write(path, format!("{DEFAULT_HEADER}\n{text}"))
                .with_context(|| format!("writing {}", path.display()))?;
            Ok((cfg, true))
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self.stt.backend.as_str() {
            "whisper" | "openai" => {}
            other => anyhow::bail!("stt.backend must be \"whisper\" or \"openai\" (got {other:?})"),
        }
        match self.format.backend.as_str() {
            "none" | "claude" | "openai" => {}
            other => anyhow::bail!("format.backend must be \"none\", \"claude\" or \"openai\" (got {other:?})"),
        }
        if self.hotkey.chord.is_empty() {
            anyhow::bail!("hotkey.chord must contain at least one key");
        }
        Ok(())
    }
}

/// Resolves a secret from the inline value first, then the named environment variable.
pub fn resolve_secret(inline: &str, env_name: &str) -> Option<String> {
    if !inline.trim().is_empty() {
        return Some(inline.trim().to_string());
    }
    std::env::var(env_name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

const DEFAULT_HEADER: &str = r#"# Dictate configuration.
#
# hotkey.chord      keys held together to dictate (push-to-talk). Tap twice quickly to lock
#                   hands-free mode; press again to stop. Esc cancels.
# stt.backend       "whisper" runs whisper.cpp locally (model downloaded on first use),
#                   "openai" sends audio to the OpenAI transcription API.
# format.backend    "none" pastes the raw transcript; "claude" or "openai" clean it up with an LLM.
# API keys can be given inline (api_key) or through the environment variable named in api_key_env.
"#;
