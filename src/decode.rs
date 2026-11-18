//! Turning a file into something to play and something to draw.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::proto::{FRAME, RATE};

pub struct Track {
    /// The whole file as 48 kHz stereo s16le.
    pub pcm: Arc<Vec<u8>>,
    pub frames: u64,
    pub envelope: Envelope,
}

/// Decode with ffmpeg, which handles any format and does the resampling.
pub fn decode(path: &Path) -> Result<Track> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(url(path))
        .args(["-map", "0:a:0", "-f", "s16le", "-ac", "2", "-ar"])
        .arg(RATE.to_string())
        .arg("pipe:1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => anyhow::anyhow!("ffmpeg is not installed on this machine"),
            _ => e.into(),
        })
        .context("cannot run ffmpeg")?;

    let mut pcm = out.stdout;
    pcm.truncate(pcm.len() / FRAME * FRAME);
    if pcm.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // ffmpeg's last line is the summary; drop the "file:…: " it prefixes.
        let reason = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no audio found");
        let reason = reason.rsplit(": ").next().unwrap_or(reason);
        bail!("{}", reason.trim());
    }

    Ok(Track {
        frames: (pcm.len() / FRAME) as u64,
        envelope: Envelope::of(&pcm),
        pcm: Arc::new(pcm),
    })
}

/// Paths go to ffmpeg as `file:` URLs so names with colons or leading dashes are safe.
fn url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path);
    url
}

/// Loudness over time at a fixed resolution, from which bars of any count are cut.
pub struct Envelope {
    /// Mean square of each slice of the file.
    power: Vec<f32>,
}

impl Envelope {
    const SLICES: usize = 4096;

    pub fn of(pcm: &[u8]) -> Envelope {
        let frames = pcm.len() / FRAME;
        let slices = Self::SLICES.min(frames);
        let power = (0..slices)
            .map(|i| {
                let (lo, hi) = (i * frames / slices, (i + 1) * frames / slices);
                let samples = pcm[lo * FRAME..hi * FRAME].as_chunks::<2>().0;
                let sum: f64 = samples
                    .iter()
                    .map(|&s| {
                        let v = i16::from_le_bytes(s) as f64 / 32768.0;
                        v * v
                    })
                    .sum();
                (sum / ((hi - lo) * 2) as f64) as f32
            })
            .collect();
        Envelope { power }
    }

    /// `n` bar heights in 0..=1, scaled so the loudest bar is full height.
    pub fn bars(&self, n: usize) -> Vec<f32> {
        let slices = self.power.len();
        if slices == 0 {
            return vec![0.0; n];
        }
        let mut bars: Vec<f32> = (0..n)
            .map(|i| {
                let lo = i * slices / n;
                let hi = ((i + 1) * slices).div_ceil(n).clamp(lo + 1, slices);
                let mean = self.power[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
                mean.sqrt()
            })
            .collect();
        let loudest = bars.iter().copied().fold(0.0, f32::max);
        for bar in &mut bars {
            // A gentle curve keeps quiet passages visible next to loud ones.
            *bar = if loudest > 1e-4 {
                (*bar / loudest).powf(0.7)
            } else {
                0.0
            };
        }
        bars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(levels: &[i16], frames_each: usize) -> Vec<u8> {
        levels
            .iter()
            .flat_map(|&v| std::iter::repeat_n(v, frames_each * 2))
            .flat_map(i16::to_le_bytes)
            .collect()
    }

    #[test]
    fn bars_follow_loudness() {
        let env = Envelope::of(&pcm(&[0, 8000, 16000, 0], 10_000));
        let bars = env.bars(4);
        assert_eq!(bars[0], 0.0);
        assert!(bars[1] > 0.3 && bars[1] < bars[2]);
        assert_eq!(bars[2], 1.0);
        assert_eq!(bars[3], 0.0);
    }

    #[test]
    fn any_bar_count_works_even_for_tiny_files() {
        let env = Envelope::of(&pcm(&[1000], 3));
        for n in [1, 2, 3, 50, 500] {
            let bars = env.bars(n);
            assert_eq!(bars.len(), n);
            assert!(bars.iter().all(|&b| (b - 1.0).abs() < 1e-6), "{bars:?}");
        }
        assert_eq!(Envelope::of(&[]).bars(10), vec![0.0; 10]);
        assert_eq!(Envelope::of(&pcm(&[0], 100)).bars(3), vec![0.0; 3]);
    }
}
