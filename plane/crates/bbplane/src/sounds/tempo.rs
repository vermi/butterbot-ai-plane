//! The beat of music the robot hears, for dancing to it.
//!
//! Loudness every 20 ms; where it jumps (an onset: a drum hit, a strum) counts; the spacing that
//! lines the onsets up best over the last eight seconds is the beat. Plain autocorrelation: good
//! enough for anything with a steady beat, which is what one dances to.

use std::collections::VecDeque;

/// Microphone samples a second.
const RATE: usize = 16_000;
/// One loudness value per hop: 50 a second.
const HOP: usize = RATE / 50;
const FRAMES_PER_SEC: f64 = 50.0;
/// Onsets kept: eight seconds.
const KEEP: usize = 400;
/// Tempo range looked at, in beats a minute.
const MIN_BPM: f64 = 70.0;
const MAX_BPM: f64 = 180.0;
/// Smoothing over neighbouring steps.
const SMOOTH: [f64; 5] = [1.0 / 9.0, 2.0 / 9.0, 3.0 / 9.0, 2.0 / 9.0, 1.0 / 9.0];
/// A beat must repeat at least this well (autocorrelation, 1 = perfectly).
const MIN_CORRELATION: f64 = 0.2;
/// The fastest spacing that repeats nearly as well as the best one is the beat (the best is often
/// two beats).
const OCTAVE_SHARE: f64 = 0.8;

#[derive(Default)]
pub struct Tempo {
    partial: Vec<f32>,
    last_level: Option<f32>,
    onsets: VecDeque<f32>,
}

impl Tempo {
    pub fn push(&mut self, samples: &[i16]) {
        for &s in samples {
            self.partial.push(s as f32 / 32768.0);
            if self.partial.len() == HOP {
                let energy = self.partial.iter().map(|x| x * x).sum::<f32>() / HOP as f32;
                self.partial.clear();
                let level = (energy + 1e-9).ln();
                let onset = self.last_level.map_or(0.0, |l| (level - l).max(0.0));
                self.last_level = Some(level);
                if self.onsets.len() == KEEP {
                    self.onsets.pop_front();
                }
                self.onsets.push_back(onset);
            }
        }
    }

    /// Start over (the robot spoke, or the music changed).
    pub fn reset(&mut self) {
        self.partial.clear();
        self.last_level = None;
        self.onsets.clear();
    }

    /// The beat in beats a minute, if there is a clear one in the last few seconds.
    pub fn bpm(&self) -> Option<f64> {
        if self.onsets.len() < KEEP / 2 {
            return None;
        }
        // Smoothed a little: a beat rarely falls on a whole number of 20 ms steps.
        let raw: Vec<f64> = self.onsets.iter().map(|&v| v as f64).collect();
        let x: Vec<f64> = (0..raw.len())
            .map(|i| {
                SMOOTH
                    .iter()
                    .enumerate()
                    .filter_map(|(k, w)| {
                        raw.get((i + k).checked_sub(SMOOTH.len() / 2)?)
                            .map(|v| v * w)
                    })
                    .sum::<f64>()
            })
            .collect();
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        let x: Vec<f64> = x.iter().map(|v| v - mean).collect();
        let r0 = x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64;
        if r0 <= 1e-12 {
            return None;
        }
        // Autocorrelation against the signal's own strength: 1 is a perfect repeat.
        let r = |lag: usize| -> f64 {
            x.iter().zip(&x[lag..]).map(|(a, b)| a * b).sum::<f64>() / (x.len() - lag) as f64 / r0
        };
        let lag_lo = (60.0 * FRAMES_PER_SEC / MAX_BPM).floor() as usize;
        let lag_hi = (60.0 * FRAMES_PER_SEC / MIN_BPM).ceil() as usize;
        let scores: Vec<f64> = (lag_lo - 1..=lag_hi + 1).map(r).collect();
        let at = |lag: usize| scores[lag + 1 - lag_lo];
        let best = (lag_lo..=lag_hi).map(at).fold(f64::MIN, f64::max);
        if best < MIN_CORRELATION {
            return None;
        }
        // A beat repeats at twice its spacing too: take the fastest strong one.
        let lag = (lag_lo..=lag_hi)
            .find(|&l| at(l) >= best * OCTAVE_SHARE && at(l) >= at(l - 1) && at(l) >= at(l + 1))?;
        // Refine between neighbouring lags.
        let (a, b, c) = (at(lag - 1), at(lag), at(lag + 1));
        let shift = if a - 2.0 * b + c != 0.0 {
            (0.5 * (a - c) / (a - 2.0 * b + c)).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        Some(60.0 * FRAMES_PER_SEC / (lag as f64 + shift))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clicks at `bpm` over quiet noise, `secs` long.
    fn clicks(bpm: f64, secs: f64) -> Vec<i16> {
        let n = (RATE as f64 * secs) as usize;
        let period = (60.0 / bpm * RATE as f64) as usize;
        let mut rng = 12345u64;
        (0..n)
            .map(|i| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let noise = (rng % 200) as i16 - 100;
                if i % period < 400 { 12_000 } else { noise }
            })
            .collect()
    }

    #[test]
    fn finds_a_steady_beat() {
        for bpm in [90.0, 120.0, 140.0] {
            let mut t = Tempo::default();
            t.push(&clicks(bpm, 9.0));
            let got = t.bpm().expect("a beat");
            assert!((got - bpm).abs() < 4.0, "{bpm}: got {got}");
        }
    }

    #[test]
    fn no_beat_in_noise_or_too_little_audio() {
        let mut t = Tempo::default();
        t.push(&clicks(120.0, 2.0));
        assert!(t.bpm().is_none(), "too short");
        let mut quiet = Tempo::default();
        let mut rng = 99u64;
        let noise: Vec<i16> = (0..RATE * 9)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (rng % 2000) as i16 - 1000
            })
            .collect();
        quiet.push(&noise);
        assert!(quiet.bpm().is_none());
    }
}
