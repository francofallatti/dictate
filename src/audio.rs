//! Microphone capture with cpal, converted on the fly to 16 kHz mono f32 (what whisper expects).

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, SizedSample, Stream, SupportedStreamConfig};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

pub const TARGET_RATE: u32 = 16_000;

/// Human-readable device name (cpal 0.18 moved it into `DeviceDescription`).
fn device_name(d: &cpal::Device) -> String {
    d.description().map(|desc| desc.name().to_string()).unwrap_or_else(|_| "unknown".into())
}

/// Linear-interpolating downsampler with a box pre-filter. Good enough for speech.
pub struct Downsampler {
    ratio: f64,
    pos: f64,
    prev: f32,
    have_prev: bool,
    box_len: usize,
    box_buf: Vec<f32>,
}

impl Downsampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let ratio = in_rate as f64 / out_rate as f64;
        let box_len = ratio.floor().max(1.0) as usize;
        Self { ratio, pos: 0.0, prev: 0.0, have_prev: false, box_len, box_buf: Vec::with_capacity(box_len) }
    }

    /// Feeds mono samples at the input rate, appending converted samples to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.ratio - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        // Box average groups of `box_len` input samples (anti-aliasing), then interpolate.
        for &s in input {
            self.box_buf.push(s);
            if self.box_buf.len() < self.box_len {
                continue;
            }
            let avg = self.box_buf.iter().sum::<f32>() / self.box_buf.len() as f32;
            self.box_buf.clear();
            self.push_filtered(avg, out);
        }
    }

    fn push_filtered(&mut self, s: f32, out: &mut Vec<f32>) {
        // The filtered stream runs at in_rate / box_len; interpolate to the output rate.
        let eff_ratio = self.ratio / self.box_len as f64;
        if !self.have_prev {
            self.prev = s;
            self.have_prev = true;
            self.pos = 0.0;
            return;
        }
        // Output samples fall at positions pos, pos+eff_ratio, ... within [prev, s].
        while self.pos < 1.0 {
            let v = self.prev as f64 + (s as f64 - self.prev as f64) * self.pos;
            out.push(v as f32);
            self.pos += eff_ratio;
        }
        self.pos -= 1.0;
        self.prev = s;
    }
}

pub struct Recorder {
    device: cpal::Device,
    config: SupportedStreamConfig,
    stream: Option<Stream>,
    buffer: Arc<Mutex<Vec<f32>>>,
}

impl Recorder {
    pub fn new(device_filter: &str) -> Result<Self> {
        let host = cpal::default_host();
        let device = if device_filter.trim().is_empty() {
            host.default_input_device().ok_or_else(|| anyhow!("no default input device"))?
        } else {
            let wanted = device_filter.to_lowercase();
            host.input_devices()?
                .find(|d| device_name(d).to_lowercase().contains(&wanted))
                .ok_or_else(|| anyhow!("no input device matching {device_filter:?}"))?
        };
        let config = device.default_input_config().context("querying default input config")?;
        info!(
            device = %device_name(&device),
            rate = config.sample_rate(),
            channels = config.channels(),
            format = ?config.sample_format(),
            "input device ready"
        );
        Ok(Self { device, config, stream: None, buffer: Arc::new(Mutex::new(Vec::new())) })
    }

    pub fn device_name(&self) -> String {
        device_name(&self.device)
    }

    pub fn is_recording(&self) -> bool {
        self.stream.is_some()
    }

    pub fn start(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        self.buffer.lock().unwrap().clear();
        let stream = match self.config.sample_format() {
            SampleFormat::F32 => self.build::<f32>()?,
            SampleFormat::I16 => self.build::<i16>()?,
            SampleFormat::U16 => self.build::<u16>()?,
            SampleFormat::I32 => self.build::<i32>()?,
            SampleFormat::U8 => self.build::<u8>()?,
            other => anyhow::bail!("unsupported sample format {other:?}"),
        };
        stream.play().context("starting input stream")?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Stops capture and returns the 16 kHz mono recording.
    pub fn stop(&mut self) -> Vec<f32> {
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
        std::mem::take(&mut *self.buffer.lock().unwrap())
    }

    fn build<T>(&self) -> Result<Stream>
    where
        T: SizedSample,
        f32: cpal::FromSample<T>,
    {
        let channels = self.config.channels() as usize;
        let mut down = Downsampler::new(self.config.sample_rate(), TARGET_RATE);
        let buffer = Arc::clone(&self.buffer);
        let mut mono: Vec<f32> = Vec::new();
        let mut converted: Vec<f32> = Vec::new();
        let stream = self.device.build_input_stream(
            self.config.config(),
            move |data: &[T], _| {
                mono.clear();
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    mono.push(sum / channels as f32);
                }
                converted.clear();
                down.process(&mono, &mut converted);
                if let Ok(mut b) = buffer.lock() {
                    b.extend_from_slice(&converted);
                }
            },
            |err| warn!(error = %err, "input stream error"),
            None,
        )?;
        Ok(stream)
    }
}

/// Plain device names, for the settings panel.
pub fn input_device_names() -> Result<Vec<String>> {
    let host = cpal::default_host();
    Ok(host.input_devices()?.map(|d| device_name(&d)).collect())
}

/// Lists input devices for `--list-devices`.
pub fn list_input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let default = host.default_input_device().map(|d| device_name(&d));
    let mut names = Vec::new();
    for d in host.input_devices()? {
        let name = device_name(&d);
        let mark = if Some(&name) == default.as_ref() { " (default)" } else { "" };
        let cfg = d.default_input_config().map(|c| format!("{} Hz, {} ch", c.sample_rate(), c.channels())).unwrap_or_default();
        names.push(format!("{name}{mark}  [{cfg}]"));
    }
    Ok(names)
}

/// Peak-normalised RMS in dBFS, used for silence checks.
pub fn rms_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let mean_sq = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    20.0 * mean_sq.sqrt().max(1e-9).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsamples_48k_to_16k_length() {
        let mut d = Downsampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        d.process(&input, &mut out);
        assert!((out.len() as i64 - 16_000).abs() < 4, "got {}", out.len());
    }

    #[test]
    fn passthrough_at_16k() {
        let mut d = Downsampler::new(16_000, 16_000);
        let input = vec![0.5f32; 100];
        let mut out = Vec::new();
        d.process(&input, &mut out);
        assert_eq!(out.len(), 100);
    }

    #[test]
    fn downsamples_44k1_to_16k_length() {
        let mut d = Downsampler::new(44_100, 16_000);
        let input = vec![0.1f32; 44_100];
        let mut out = Vec::new();
        d.process(&input, &mut out);
        assert!((out.len() as i64 - 16_000).abs() < 8, "got {}", out.len());
    }
}
