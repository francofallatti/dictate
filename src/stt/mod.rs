//! Speech-to-text backends behind one trait.

use crate::config::{Config, Paths};
use anyhow::Result;
use std::sync::Arc;

pub mod openai;
#[cfg(feature = "whisper")]
pub mod whisper;

/// Hints shared by all backends.
#[derive(Debug, Clone, Default)]
pub struct Hint {
    /// ISO 639-1 code or "auto".
    pub language: String,
    /// Vocabulary / style hint.
    pub initial_prompt: String,
}

pub trait Transcriber: Send + Sync {
    fn name(&self) -> &'static str;
    /// Loads models / checks credentials ahead of the first dictation.
    fn warm_up(&self) -> Result<()> {
        Ok(())
    }
    /// `pcm` is 16 kHz mono in [-1, 1].
    fn transcribe(&self, pcm: &[f32], hint: &Hint) -> Result<String>;
}

pub fn build(cfg: &Config, paths: &Paths) -> Result<Arc<dyn Transcriber>> {
    match cfg.stt.backend.as_str() {
        "openai" => Ok(Arc::new(openai::OpenAiTranscriber::new(&cfg.stt.openai)?)),
        "whisper" => {
            #[cfg(feature = "whisper")]
            {
                Ok(Arc::new(whisper::WhisperTranscriber::new(&cfg.stt.whisper, paths)?))
            }
            #[cfg(not(feature = "whisper"))]
            {
                let _ = paths;
                anyhow::bail!("this build has no local whisper support; set stt.backend = \"openai\" or rebuild with --features whisper")
            }
        }
        other => anyhow::bail!("unknown stt backend {other:?}"),
    }
}

/// Encodes 16 kHz mono f32 PCM as a 16-bit WAV file in memory.
pub fn wav_bytes(pcm: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for &s in pcm {
            writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Reads any WAV file into 16 kHz mono f32 (used by `--transcribe`).
pub fn read_wav_16k(path: &std::path::Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let max = ((1u64 << (spec.bits_per_sample - 1)) - 1) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 / max)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = samples
        .chunks(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    let mut out = Vec::new();
    crate::audio::Downsampler::new(spec.sample_rate, crate::audio::TARGET_RATE).process(&mono, &mut out);
    Ok(out)
}

/// Whisper-style post-cleaning shared by backends: trims, collapses whitespace, drops
/// bracketed non-speech tags like `[BLANK_AUDIO]` or `(música)`.
pub fn clean_transcript(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth_sq = 0i32;
    let mut depth_par = 0i32;
    for ch in raw.chars() {
        match ch {
            '[' => depth_sq += 1,
            ']' => depth_sq = (depth_sq - 1).max(0),
            '(' => depth_par += 1,
            ')' => depth_par = (depth_par - 1).max(0),
            _ if depth_sq == 0 && depth_par == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_tags_and_whitespace() {
        assert_eq!(clean_transcript(" hola  [BLANK_AUDIO] mundo (risas) . "), "hola mundo .");
        assert_eq!(clean_transcript("[BLANK_AUDIO]"), "");
    }

    #[test]
    fn wav_roundtrip() {
        let pcm: Vec<f32> = (0..1600).map(|i| (i as f32 / 10.0).sin() * 0.5).collect();
        let bytes = wav_bytes(&pcm, 16_000).unwrap();
        let reader = hound::WavReader::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.len(), 1600);
    }
}
