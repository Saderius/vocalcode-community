//! Debug-only audio source: replay a WAV file as if it came from the microphone.
//!
//! This exists so a dictation can be exercised end to end — capture, recognition,
//! delivery — on a machine with no one speaking into it, and in CI. It is
//! compiled out of release builds and only selected when `VOCALCODE_AUDIO_WAV`
//! names a file, so it can never affect a real installation.
//!
//! The parser is deliberately small: the only producer is a test harness writing
//! 16-bit PCM or 32-bit float, and a malformed file should fail loudly rather
//! than be guessed at.

use std::path::Path;

use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::{AudioCapture, Recording};

pub struct WavCapture {
    samples: Vec<f32>,
    sample_rate: u32,
    recording: bool,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Read a RIFF/WAVE file as mono `f32` samples plus its rate.
fn read_wav(path: &Path) -> anyhow::Result<(Vec<f32>, u32)> {
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(
        bytes.len() >= 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "not a RIFF/WAVE file"
    );

    let (mut format, mut channels, mut rate, mut bits) = (1_u16, 1_u16, 16_000_u32, 16_u16);
    let mut data: Option<&[u8]> = None;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(&bytes, pos + 4) as usize;
        let body = pos + 8;
        let end = (body + size).min(bytes.len());
        match id {
            b"fmt " => {
                anyhow::ensure!(size >= 16, "short fmt chunk");
                format = u16_at(&bytes, body);
                channels = u16_at(&bytes, body + 2).max(1);
                rate = u32_at(&bytes, body + 4);
                bits = u16_at(&bytes, body + 14);
            }
            b"data" => data = Some(&bytes[body..end]),
            _ => {}
        }
        // Chunks are word-aligned.
        pos = body + size + (size & 1);
    }

    let data = data.ok_or_else(|| anyhow::anyhow!("the WAV file has no data chunk"))?;
    let channels = usize::from(channels);
    let mut mono = Vec::new();
    match (format, bits) {
        // 1 = PCM integer
        (1, 16) => {
            for frame in data.chunks_exact(2 * channels) {
                let mut sum = 0.0_f32;
                for channel in 0..channels {
                    let at = channel * 2;
                    sum += f32::from(i16::from_le_bytes([frame[at], frame[at + 1]])) / 32_768.0;
                }
                mono.push(sum / channels as f32);
            }
        }
        // 3 = IEEE float
        (3, 32) => {
            for frame in data.chunks_exact(4 * channels) {
                let mut sum = 0.0_f32;
                for channel in 0..channels {
                    let at = channel * 4;
                    sum += f32::from_le_bytes([
                        frame[at],
                        frame[at + 1],
                        frame[at + 2],
                        frame[at + 3],
                    ]);
                }
                mono.push(sum / channels as f32);
            }
        }
        _ => anyhow::bail!("unsupported WAV format {format} with {bits} bits per sample"),
    }
    anyhow::ensure!(!mono.is_empty(), "the WAV file contains no audio");
    Ok((mono, rate))
}

impl WavCapture {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let (samples, sample_rate) = read_wav(path)?;
        Ok(Self {
            samples,
            sample_rate,
            recording: false,
        })
    }
}

impl AudioCapture for WavCapture {
    fn start(&mut self) -> Result<()> {
        self.recording = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<Recording> {
        if !self.recording {
            return Err(VocalCodeError::Audio("not recording".into()));
        }
        self.recording = false;
        Ok(Recording {
            samples: self.samples.clone(),
            sample_rate: self.sample_rate,
        })
    }

    fn is_recording(&self) -> bool {
        self.recording
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, samples: &[i16], rate: u32) {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_pcm_wav_is_read_back_as_normalised_samples() {
        let dir = std::env::temp_dir().join(format!("vc-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.wav");
        write_wav(&path, &[0, 16_384, -16_384, 32_767], 16_000);
        let (samples, rate) = read_wav(&path).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 4);
        assert!((samples[1] - 0.5).abs() < 0.001);
        assert!((samples[2] + 0.5).abs() < 0.001);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
