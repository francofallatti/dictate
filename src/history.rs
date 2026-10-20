//! Append-only JSONL history of dictations.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

/// Owned form used by the panel.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HistoryRecord {
    pub timestamp: String,
    pub app: String,
    pub window_title: String,
    pub audio_secs: f32,
    pub stt_backend: String,
    pub format_backend: String,
    pub raw_text: String,
    pub final_text: String,
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

/// Reads every record in file order (oldest first); malformed lines are skipped.
pub fn read_all(path: &Path) -> Result<Vec<HistoryRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<HistoryRecord>(l).ok())
        .collect())
}

/// Removes the record(s) with the given timestamp by rewriting the file.
pub fn delete(path: &Path, timestamp: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            serde_json::from_str::<HistoryRecord>(l)
                .map(|r| r.timestamp != timestamp)
                .unwrap_or(true)
        })
        .collect();
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
