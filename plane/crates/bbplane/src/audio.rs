//! PCM helpers: WAV in/out, mono mixdown, resampling, level stats.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const RATE: u32 = bbproto::SAMPLE_RATE;

/// Encode 16 kHz mono PCM as a WAV file in memory.
pub fn wav_bytes(samples: &[i16], rate: u32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cur = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut w = hound::WavWriter::new(&mut cur, spec).expect("in-memory wav");
        for s in samples {
            w.write_sample(*s).expect("in-memory wav");
        }
        w.finalize().expect("in-memory wav");
    }
    cur.into_inner()
}

/// Read any PCM WAV and return it as 16 kHz mono i16.
pub fn read_wav_16k(bytes: &[u8]) -> Result<Vec<i16>> {
    let mut r = hound::WavReader::new(Cursor::new(bytes)).context("not a WAV file")?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    if interleaved.is_empty() {
        bail!("WAV has no samples");
    }
    let mono: Vec<f32> = interleaved
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / f.len() as f32)
        .collect();
    let out = resample(&mono, spec.sample_rate, RATE);
    Ok(out
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect())
}

pub fn read_wav_file_16k(path: &Path) -> Result<Vec<i16>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    read_wav_16k(&bytes)
}

/// Windowed-sinc resampler. Quality is plenty for speech; not meant for music mastering.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    // Low-pass at the lower Nyquist when downsampling.
    let cutoff = ratio.min(1.0) * 0.95;
    const HALF: i64 = 16;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 / ratio;
        let center = pos.floor() as i64;
        let mut acc = 0.0f64;
        let mut wsum = 0.0f64;
        for k in (center - HALF + 1)..=(center + HALF) {
            if k < 0 || k as usize >= input.len() {
                continue;
            }
            let x = pos - k as f64;
            let sinc = if x.abs() < 1e-9 {
                1.0
            } else {
                (std::f64::consts::PI * x * cutoff).sin() / (std::f64::consts::PI * x * cutoff)
            };
            // Blackman window over the kernel span.
            let t = (x + HALF as f64) / (2.0 * HALF as f64);
            let w = if (0.0..=1.0).contains(&t) {
                0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * t).cos()
            } else {
                0.0
            };
            let c = sinc * w;
            acc += input[k as usize] as f64 * c;
            wsum += c;
        }
        out.push(if wsum.abs() > 1e-9 {
            (acc / wsum) as f32
        } else {
            0.0
        });
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct LevelStats {
    pub seconds: f32,
    /// RMS level in dBFS over the whole clip.
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    /// Fraction of 32 ms windows that are well above the clip's noise floor or louder than -35 dBFS.
    pub active_ratio: f32,
    /// A rough guess whether the clip contains sound beyond background noise.
    pub likely_sound: bool,
}

fn dbfs(v: f64) -> f32 {
    if v <= 1e-9 {
        -120.0
    } else {
        (20.0 * (v / i16::MAX as f64).log10()) as f32
    }
}

pub fn level_stats(samples: &[i16]) -> LevelStats {
    let seconds = samples.len() as f32 / RATE as f32;
    if samples.is_empty() {
        return LevelStats {
            seconds,
            rms_dbfs: -120.0,
            peak_dbfs: -120.0,
            active_ratio: 0.0,
            likely_sound: false,
        };
    }
    let sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sq / samples.len() as f64).sqrt();
    let peak = samples
        .iter()
        .map(|s| (*s as i32).unsigned_abs())
        .max()
        .unwrap_or(0) as f64;
    let win = (RATE as usize * 32) / 1000;
    let mut energies: Vec<f64> = samples
        .chunks(win)
        .map(|c| (c.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / c.len() as f64).sqrt())
        .collect();
    let n = energies.len();
    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = sorted[n / 10].max(30.0);
    // -35 dBFS: clearly audible regardless of how steady the sound is.
    let loud = i16::MAX as f64 * 10f64.powf(-35.0 / 20.0);
    let active = energies
        .iter_mut()
        .filter(|e| **e > floor * 4.0 || **e > loud)
        .count();
    let active_ratio = active as f32 / n as f32;
    LevelStats {
        seconds,
        rms_dbfs: dbfs(rms),
        peak_dbfs: dbfs(peak),
        active_ratio,
        likely_sound: active_ratio > 0.05 && dbfs(peak) > -40.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn resample_keeps_tone_and_length() {
        let x = tone(440.0, 22050, 0.5, 0.5);
        let y = resample(&x, 22050, 16000);
        assert_eq!(y.len(), 8000);
        // Count zero crossings: 440 Hz over 0.5 s is about 440 crossings.
        let zc = y
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        assert!((430..=450).contains(&zc), "zero crossings {zc}");
        let peak = y[1000..7000].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((0.45..0.55).contains(&peak), "peak {peak}");
    }

    #[test]
    fn downsample_removes_aliasing_tone() {
        // 10 kHz is above the 8 kHz Nyquist of 16 kHz output and must be attenuated.
        let x = tone(10_000.0, 48000, 0.2, 0.5);
        let y = resample(&x, 48000, 16000);
        let peak = y[200..y.len() - 200]
            .iter()
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.05, "aliased peak {peak}");
    }

    #[test]
    fn wav_roundtrip_and_stereo_mixdown() {
        let s: Vec<i16> = (0..1600).map(|i| (i * 10) as i16).collect();
        let back = read_wav_16k(&wav_bytes(&s, RATE)).unwrap();
        assert_eq!(back.len(), s.len());
        assert!((back[800] as i32 - s[800] as i32).abs() <= 1);

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cur = Cursor::new(Vec::new());
        let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
        for _ in 0..100 {
            w.write_sample(1000i16).unwrap();
            w.write_sample(3000i16).unwrap();
        }
        w.finalize().unwrap();
        let m = read_wav_16k(&cur.into_inner()).unwrap();
        assert_eq!(m.len(), 100);
        assert!((m[50] - 2000).abs() <= 1);
    }

    #[test]
    fn stats_detect_sound() {
        let silence = vec![0i16; 16000];
        assert!(!level_stats(&silence).likely_sound);
        let mut clip: Vec<i16> = (0..16000).map(|i| ((i * 7919) % 41) as i16 - 20).collect();
        for (i, s) in clip.iter_mut().enumerate().skip(4000).take(4000) {
            *s = (8000.0 * (i as f32 * 0.2).sin()) as i16;
        }
        let st = level_stats(&clip);
        assert!(st.likely_sound, "{st:?}");
        assert!(st.active_ratio > 0.2 && st.active_ratio < 0.3, "{st:?}");
    }
}
