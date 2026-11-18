//! The listener's audio source: a queue of frames received from the player,
//! pulled by the audio device.
//!
//! It also keeps track of what has actually been heard. Frames leave the queue
//! some milliseconds before they reach the speakers, so the playhead reported
//! back to the player is the queue position as of `latency` frames of output ago.

use std::collections::VecDeque;

use crate::proto::{RATE, Report};

/// Fade length for pause, resume and seek, to avoid clicks.
const FADE_SECS: f32 = 0.006;

/// How much queued audio the listener accepts before it stops reading.
pub const MAX_QUEUE: usize = RATE as usize * 2;

struct Mark {
    /// Device frames produced up to and including this fill.
    out_end: u64,
    epoch: u32,
    consumed: u64,
}

pub struct Source {
    queue: VecDeque<[i16; 2]>,
    epoch: u32,
    consumed: u64,
    paused: bool,
    volume: f32,
    gain: f32,
    // Resampler state, used only when the device does not run at 48 kHz.
    hist: [[f32; 2]; 4],
    phase: f64,
    // Heard-position bookkeeping.
    out_total: u64,
    latency: u64,
    marks: VecDeque<Mark>,
}

impl Source {
    pub fn new() -> Source {
        Source {
            queue: VecDeque::new(),
            epoch: 0,
            consumed: 0,
            paused: true,
            volume: 1.0,
            gain: 0.0,
            hist: [[0.0; 2]; 4],
            phase: 0.0,
            out_total: 0,
            latency: 0,
            marks: VecDeque::new(),
        }
    }

    /// Append s16le stereo PCM.
    pub fn push(&mut self, pcm: &[u8]) {
        let frames = pcm.as_chunks::<4>().0.iter();
        self.queue.extend(frames.map(|f| [i16::from_le_bytes([f[0], f[1]]), i16::from_le_bytes([f[2], f[3]])]));
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn flush(&mut self, epoch: u32) {
        self.queue.clear();
        self.epoch = epoch;
        self.consumed = 0;
        self.gain = 0.0;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
    }

    /// False once paused and fully faded out: the device can be stopped.
    pub fn wants_output(&self) -> bool {
        !self.paused || self.gain > 0.0
    }

    pub fn report(&self) -> Report {
        let heard = self.out_total.saturating_sub(self.latency);
        let played = self
            .marks
            .iter()
            .take_while(|m| m.out_end <= heard)
            .last()
            .filter(|m| m.epoch == self.epoch)
            .map_or(0, |m| m.consumed);
        Report { epoch: self.epoch, consumed: self.consumed, played }
    }

    /// Fill `out` (interleaved, `channels` wide, at `rate` Hz) with the next audio.
    /// `latency` is how many device frames lie between this buffer and the speakers.
    pub fn fill(&mut self, out: &mut [f32], channels: usize, rate: u32, latency: u64) {
        let channels = channels.max(1);
        let rate = rate.max(1);
        let step = RATE as f64 / rate as f64;
        let ramp = 1.0 / (FADE_SECS * rate as f32);
        let mut frames = 0u64;

        for frame in out.chunks_exact_mut(channels) {
            frames += 1;
            let target = if self.paused { 0.0 } else { self.volume };
            if self.paused && self.gain == 0.0 {
                frame.fill(0.0);
                continue;
            }
            self.gain = if self.gain < target {
                (self.gain + ramp).min(target)
            } else {
                (self.gain - ramp).max(target)
            };
            let [l, r] = if rate == RATE { self.next_direct() } else { self.next_resampled(step) };
            let (l, r) = (l * self.gain, r * self.gain);
            if channels == 1 {
                frame[0] = (l + r) * 0.5;
            } else {
                frame[0] = l;
                frame[1] = r;
                frame[2..].fill(0.0);
            }
        }

        self.out_total += frames;
        self.latency = latency;
        self.marks.push_back(Mark {
            out_end: self.out_total,
            epoch: self.epoch,
            consumed: self.consumed,
        });
        // Keep only the newest mark that has already been heard, plus everything after it.
        let heard = self.out_total.saturating_sub(latency);
        while self.marks.len() > 4096 || self.marks.get(1).is_some_and(|m| m.out_end <= heard) {
            self.marks.pop_front();
        }
    }

    fn pop(&mut self) -> Option<[f32; 2]> {
        let [l, r] = self.queue.pop_front()?;
        self.consumed += 1;
        Some([l as f32 / 32768.0, r as f32 / 32768.0])
    }

    fn next_direct(&mut self) -> [f32; 2] {
        self.pop().unwrap_or_else(|| {
            // Underrun: play silence and fade back in when audio returns.
            self.gain = 0.0;
            [0.0; 2]
        })
    }

    /// Four-point Hermite interpolation between `hist[1]` and `hist[2]`.
    fn next_resampled(&mut self, step: f64) -> [f32; 2] {
        while self.phase >= 1.0 {
            let next = self.pop().unwrap_or([0.0; 2]);
            self.hist = [self.hist[1], self.hist[2], self.hist[3], next];
            self.phase -= 1.0;
        }
        let t = self.phase as f32;
        self.phase += step;
        let h = &self.hist;
        std::array::from_fn(|c| {
            let (y0, y1, y2, y3) = (h[0][c], h[1][c], h[2][c], h[3][c]);
            let c1 = 0.5 * (y2 - y0);
            let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
            let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
            ((c3 * t + c2) * t + c1) * t + y1
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(frames: usize, value: i16) -> Vec<u8> {
        (0..frames * 2).flat_map(|_| value.to_le_bytes()).collect()
    }

    fn playing() -> Source {
        let mut s = Source::new();
        s.set_paused(false);
        s
    }

    #[test]
    fn plays_queue_in_order_and_counts_it() {
        let mut s = playing();
        s.push(&pcm(1000, 16384));
        let mut out = vec![0f32; 2 * 600];
        s.fill(&mut out, 2, RATE, 0);
        assert_eq!(s.report().consumed, 600);
        assert_eq!(s.queued(), 400);
        // Faded in to full level (16384 / 32768) by the end of the buffer.
        assert!((out[2 * 599] - 0.5).abs() < 1e-4);
        assert!(out[0] < 0.01, "starts from a fade, not a click");
    }

    #[test]
    fn underrun_is_silence_and_is_not_counted() {
        let mut s = playing();
        s.push(&pcm(100, 16384));
        let mut out = vec![1f32; 2 * 500];
        s.fill(&mut out, 2, RATE, 0);
        assert_eq!(s.report().consumed, 100);
        assert_eq!(out[2 * 499], 0.0);
    }

    #[test]
    fn pause_fades_out_then_stops_consuming() {
        let mut s = playing();
        s.push(&pcm(48_000, 16384));
        let mut out = vec![0f32; 2 * 1000];
        s.fill(&mut out, 2, RATE, 0);
        s.set_paused(true);
        assert!(s.wants_output(), "still fading");
        s.fill(&mut out, 2, RATE, 0);
        let after_fade = s.report().consumed;
        assert!(after_fade > 1000 && after_fade < 1000 + 400, "consumed {after_fade}");
        assert!(!s.wants_output());
        s.fill(&mut out, 2, RATE, 0);
        assert_eq!(s.report().consumed, after_fade);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn played_trails_consumed_by_the_device_latency() {
        let mut s = playing();
        s.push(&pcm(48_000, 1000));
        let mut out = vec![0f32; 2 * 480];
        // 960 frames of latency = two buffers.
        s.fill(&mut out, 2, RATE, 960);
        s.fill(&mut out, 2, RATE, 960);
        assert_eq!(s.report(), Report { epoch: 0, consumed: 960, played: 0 });
        s.fill(&mut out, 2, RATE, 960);
        assert_eq!(s.report().played, 480);
        s.fill(&mut out, 2, RATE, 960);
        assert_eq!(s.report().played, 960);
    }

    #[test]
    fn played_catches_up_once_the_queue_drains() {
        let mut s = playing();
        s.push(&pcm(480, 1000));
        let mut out = vec![0f32; 2 * 480];
        s.fill(&mut out, 2, RATE, 960);
        assert_eq!(s.report().played, 0);
        s.fill(&mut out, 2, RATE, 960);
        s.fill(&mut out, 2, RATE, 960);
        assert_eq!(s.report(), Report { epoch: 0, consumed: 480, played: 480 });
    }

    #[test]
    fn flush_starts_a_new_epoch_from_zero() {
        let mut s = playing();
        s.push(&pcm(2000, 1000));
        let mut out = vec![0f32; 2 * 480];
        s.fill(&mut out, 2, RATE, 480);
        s.fill(&mut out, 2, RATE, 480);
        assert_eq!(s.report().played, 480);
        s.flush(5);
        assert_eq!(s.queued(), 0);
        assert_eq!(s.report(), Report { epoch: 5, consumed: 0, played: 0 });
        s.push(&pcm(2000, 1000));
        s.fill(&mut out, 2, RATE, 480);
        // Audio from the old epoch is still in the device buffer.
        assert_eq!(s.report(), Report { epoch: 5, consumed: 480, played: 0 });
        s.fill(&mut out, 2, RATE, 480);
        assert_eq!(s.report().played, 480);
    }

    #[test]
    fn resamples_to_other_device_rates() {
        let mut s = playing();
        // One second of a 1 kHz sine at half scale.
        let sine: Vec<u8> = (0..48_000)
            .flat_map(|i| {
                let v = ((i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * 16384.0) as i16;
                [v.to_le_bytes(), v.to_le_bytes()].concat()
            })
            .collect();
        s.push(&sine);
        let mut out = vec![0f32; 2 * 22_050];
        s.fill(&mut out, 2, 44_100, 0);
        // Half a second of output uses half a second of input.
        let consumed = s.report().consumed as i64;
        assert!((consumed - 24_000).abs() <= 4, "consumed {consumed}");
        // Level is preserved and the tone is still 1 kHz: count zero crossings.
        let tail = &out[2 * 2000..];
        let peak = tail.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
        let left: Vec<f32> = tail.as_chunks::<2>().0.iter().map(|f| f[0]).collect();
        let crossings = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let expected = left.len() as f32 / 44_100.0 * 1000.0;
        assert!((crossings as f32 - expected).abs() <= 1.0, "{crossings} vs {expected}");
    }

    #[test]
    fn downmixes_and_upmixes_channels() {
        let mut s = playing();
        s.push(&pcm(2000, 16384));
        let mut mono = vec![0f32; 1000];
        s.fill(&mut mono, 1, RATE, 0);
        assert!((mono[999] - 0.5).abs() < 1e-4);
        let mut surround = vec![9f32; 6 * 100];
        s.fill(&mut surround, 6, RATE, 0);
        assert!((surround[6 * 99] - 0.5).abs() < 1e-4);
        assert_eq!(surround[6 * 99 + 2..], [0.0; 4]);
    }
}
