//! Append-only JSONL history of dictations.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct HistoryEntry<'a> {
    pub timestamp: String,
    pub app: &'a str,
    pub window_title: &'a str,
    pub audio_secs: f32,
    pub stt_backend: &'a str,
    pub format_backend: &'a str,
    pub raw_text: &'a str,
    pub final_text: &'a str,
    pub stt_ms: u64,
    pub format_ms: u64,
    pub pasted: bool,
}

pub fn append(path: &Path, entry: &HistoryEntry<'_>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    serde_json::to_writer(&mut f, entry)?;
    f.write_all(b"\n")?;
    Ok(())
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
