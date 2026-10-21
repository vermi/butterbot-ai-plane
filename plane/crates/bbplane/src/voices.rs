//! Who is talking, by voice: the robot knows people it has heard before even off camera.
//!
//! WeSpeaker's ResNet34 (trained on VoxCeleb, Apache-2.0; sherpa-onnx's ONNX export, downloaded
//! once and checked against a pinned hash) turns a few seconds of speech into a voiceprint, run with
//! tract on the laptop. The input is Kaldi-style log mel filterbanks (computed here) with the mean
//! taken out. Only speech already addressed to the robot is looked at, like the transcripts. A
//! person's voiceprint is kept (voices.json, an average of up to `MAX_SAMPLES` utterances) when
//! someone asks the robot to remember their voice, and it grows by itself from commands spoken
//! while that person is the only one the camera recognises.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tract_onnx::prelude::*;

use crate::models::{ModelFile, fetch};

const MODEL: ModelFile = ModelFile {
    max_bytes: 32 << 20,
    name: "wespeaker_en_voxceleb_resnet34.onnx",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34.onnx",
    sha256: "5ef208a9da1453335308a6b6f4e6dfbd7e183a38b604de0a57664f45d257fe94",
};

const RATE: usize = 16_000;
const FRAME: usize = 400; // 25 ms
const SHIFT: usize = 160; // 10 ms
const NFFT: usize = 512;
const MELS: usize = 80;
/// The model looks at 3 s at a time; longer speech is a few looks averaged.
const WINDOW_FRAMES: usize = 300;
const MAX_WINDOWS: usize = 3;
/// Less speech than this says too little about a voice.
const MIN_FRAMES: usize = 120;
/// Cosine similarity a voice must reach to be someone's, and its lead over the next best.
const MATCH: f32 = 0.55;
const MARGIN: f32 = 0.05;
/// A voiceprint averages at most this many utterances (newer ones count as much as older ones
/// until then; after, it keeps drifting slowly with the voice).
const MAX_SAMPLES: u32 = 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Profile {
    /// Running mean of unit-length voiceprints.
    mean: Vec<f32>,
    samples: u32,
}

pub struct Voices {
    path: PathBuf,
    models_dir: PathBuf,
    engine: OnceCell<std::sync::Arc<Engine>>,
    profiles: Mutex<BTreeMap<String, Profile>>,
    /// The latest command's voiceprint, for "remember my voice".
    last: Mutex<Option<Vec<f32>>>,
}

impl Voices {
    pub fn load(data_dir: &Path) -> Voices {
        let path = data_dir.join("voices.json");
        let profiles = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Voices {
            path,
            models_dir: data_dir.join("models"),
            engine: OnceCell::new(),
            profiles: Mutex::new(profiles),
            last: Mutex::new(None),
        }
    }

    pub fn names(&self) -> Vec<(String, u32)> {
        self.profiles
            .lock()
            .unwrap()
            .iter()
            .map(|(n, p)| (n.clone(), p.samples))
            .collect()
    }

    async fn engine(&self) -> Result<std::sync::Arc<Engine>> {
        let dir = self.models_dir.clone();
        self.engine
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || Engine::load(&dir).map(std::sync::Arc::new))
                    .await
                    .map_err(|e| anyhow!("voice model loader panicked: {e}"))?
            })
            .await
            .cloned()
    }

    /// Who said `pcm` (16 kHz), if it is a voice it knows. `faces`: the people the camera
    /// recognises right now; if that is exactly one, the voice teaches their voiceprint.
    pub async fn hear(&self, pcm: Vec<i16>, faces: &[String]) -> Option<(String, f32)> {
        let engine = match self.engine().await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("voice recognition unavailable: {e:#}");
                return None;
            }
        };
        let emb = match tokio::task::spawn_blocking(move || engine.embed(&pcm)).await {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                tracing::debug!("voice: {e:#}");
                return None;
            }
            Err(e) => {
                tracing::warn!("voice recognition panicked: {e}");
                return None;
            }
        };
        *self.last.lock().unwrap() = Some(emb.clone());
        let found = self.identify(&emb);
        // Learn from a face it knows, unless the voice says it is someone else.
        if let [face] = faces
            && found.as_ref().is_none_or(|(n, _)| n == face)
            && let Err(e) = self.learn(face, &emb)
        {
            tracing::warn!("saving voices: {e:#}");
        }
        found
    }

    fn identify(&self, emb: &[f32]) -> Option<(String, f32)> {
        let profiles = self.profiles.lock().unwrap();
        let mut scores: Vec<(&String, f32)> = profiles
            .iter()
            .map(|(n, p)| (n, cosine(emb, &p.mean)))
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (name, best) = scores.first()?;
        let next = scores.get(1).map_or(-1.0, |s| s.1);
        tracing::debug!("voice: best {name} {best:.2}, next {next:.2}");
        (*best >= MATCH && best - next >= MARGIN).then(|| ((*name).clone(), *best))
    }

    fn learn(&self, name: &str, emb: &[f32]) -> Result<()> {
        let mut profiles = self.profiles.lock().unwrap();
        let p = profiles.entry(name.to_string()).or_default();
        if p.mean.len() != emb.len() {
            p.mean = vec![0.0; emb.len()];
            p.samples = 0;
        }
        p.samples = (p.samples + 1).min(MAX_SAMPLES);
        let k = 1.0 / p.samples as f32;
        for (m, e) in p.mean.iter_mut().zip(emb) {
            *m += (e - *m) * k;
        }
        self.save(&profiles)
    }

    /// Keep the latest command's voice as `name`'s.
    pub fn remember_last(&self, name: &str) -> Result<u32> {
        let name = name.trim();
        if name.is_empty() {
            bail!("whose voice?");
        }
        let emb = self
            .last
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("I have not heard a voice to remember yet"))?;
        self.learn(name, &emb)?;
        Ok(self.profiles.lock().unwrap()[name].samples)
    }

    pub fn forget(&self, name: &str) -> Result<bool> {
        let mut profiles = self.profiles.lock().unwrap();
        let gone = profiles.remove(name.trim()).is_some();
        if gone {
            self.save(&profiles)?;
        }
        Ok(gone)
    }

    fn save(&self, profiles: &BTreeMap<String, Profile>) -> Result<()> {
        crate::config::write_private(&self.path, &serde_json::to_vec(profiles)?)
            .context("saving voices")
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

struct Engine {
    plan: Arc<TypedRunnableModel>,
    fbank: Fbank,
}

impl Engine {
    fn load(dir: &Path) -> Result<Engine> {
        let model = fetch(dir, &MODEL)?;
        let plan = tract_onnx::onnx()
            .model_for_path(&model)
            .and_then(|m| m.with_input_fact(0, f32::fact([1, WINDOW_FRAMES, MELS]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .with_context(|| format!("loading {}", model.display()))?;
        Ok(Engine {
            plan,
            fbank: Fbank::new(),
        })
    }

    /// A unit-length voiceprint of 16 kHz speech.
    fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>> {
        let feats = self.fbank.compute(pcm);
        if feats.len() < MIN_FRAMES {
            bail!("too little speech ({} frames)", feats.len());
        }
        // 3 s looks: the speech repeated to fill one, or a few spread over it.
        let windows: Vec<usize> = if feats.len() <= WINDOW_FRAMES {
            vec![0]
        } else {
            let spare = feats.len() - WINDOW_FRAMES;
            let n = MAX_WINDOWS.min(1 + spare / (WINDOW_FRAMES / 2));
            (0..n)
                .map(|i| if n == 1 { 0 } else { spare * i / (n - 1) })
                .collect()
        };
        let mut sum: Vec<f32> = Vec::new();
        for start in windows {
            let mut x = vec![0f32; WINDOW_FRAMES * MELS];
            for t in 0..WINDOW_FRAMES {
                let row = &feats[(start + t) % feats.len().max(1)];
                x[t * MELS..(t + 1) * MELS].copy_from_slice(row);
            }
            // Mean normalisation over the look, per band.
            for m in 0..MELS {
                let mean =
                    (0..WINDOW_FRAMES).map(|t| x[t * MELS + m]).sum::<f32>() / WINDOW_FRAMES as f32;
                for t in 0..WINDOW_FRAMES {
                    x[t * MELS + m] -= mean;
                }
            }
            let input = tract_ndarray::Array3::from_shape_vec((1, WINDOW_FRAMES, MELS), x)?;
            let out = self.plan.run(tvec!(Tensor::from(input).into()))?;
            let emb = out
                .first()
                .context("no output")?
                .try_as_plain_ram()?
                .as_slice::<f32>()?
                .to_vec();
            if sum.is_empty() {
                sum = vec![0.0; emb.len()];
            }
            let norm = emb.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
            for (s, e) in sum.iter_mut().zip(&emb) {
                *s += e / norm;
            }
        }
        let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        Ok(sum.iter().map(|v| v / norm).collect())
    }
}

/// Kaldi-compatible log mel filterbanks: 25 ms frames every 10 ms, DC removed, pre-emphasis 0.97,
/// Povey window, 512-point power spectrum, 80 triangular mel bands from 20 Hz to 8 kHz.
struct Fbank {
    window: Vec<f64>,
    /// Per band: the first FFT bin and the weights from there.
    bands: Vec<(usize, Vec<f64>)>,
    fft: Fft,
}

impl Fbank {
    fn new() -> Fbank {
        let window = (0..FRAME)
            .map(|i| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (FRAME - 1) as f64).cos())
                    .powf(0.85)
            })
            .collect();
        let mel = |f: f64| 1127.0 * (1.0 + f / 700.0).ln();
        let (lo, hi) = (mel(20.0), mel(RATE as f64 / 2.0));
        let delta = (hi - lo) / (MELS + 1) as f64;
        let bin_hz = RATE as f64 / NFFT as f64;
        let bands = (0..MELS)
            .map(|b| {
                let left = lo + b as f64 * delta;
                let center = left + delta;
                let right = center + delta;
                let mut first = None;
                let mut weights = Vec::new();
                for i in 0..NFFT / 2 {
                    let m = mel(bin_hz * i as f64);
                    if m > left && m < right {
                        first.get_or_insert(i);
                        weights.push(if m <= center {
                            (m - left) / (center - left)
                        } else {
                            (right - m) / (right - center)
                        });
                    } else if first.is_some() {
                        break;
                    }
                }
                (first.unwrap_or(0), weights)
            })
            .collect();
        Fbank {
            window,
            bands,
            fft: Fft::new(NFFT),
        }
    }

    fn compute(&self, pcm: &[i16]) -> Vec<[f32; MELS]> {
        if pcm.len() < FRAME {
            return Vec::new();
        }
        let frames = 1 + (pcm.len() - FRAME) / SHIFT;
        let mut out = Vec::with_capacity(frames);
        let mut re = vec![0f64; NFFT];
        let mut im = vec![0f64; NFFT];
        for f in 0..frames {
            let s = &pcm[f * SHIFT..f * SHIFT + FRAME];
            let mean = s.iter().map(|&v| v as f64).sum::<f64>() / FRAME as f64;
            let x: Vec<f64> = s.iter().map(|&v| v as f64 - mean).collect();
            re.fill(0.0);
            im.fill(0.0);
            for i in 0..FRAME {
                let prev = if i == 0 { x[0] } else { x[i - 1] };
                re[i] = (x[i] - 0.97 * prev) * self.window[i];
            }
            self.fft.run(&mut re, &mut im);
            let mut row = [0f32; MELS];
            for (b, (first, weights)) in self.bands.iter().enumerate() {
                let e: f64 = weights
                    .iter()
                    .enumerate()
                    .map(|(k, w)| {
                        let i = first + k;
                        w * (re[i] * re[i] + im[i] * im[i])
                    })
                    .sum();
                row[b] = (e.max(f32::EPSILON as f64)).ln() as f32;
            }
            out.push(row);
        }
        out
    }
}

/// A plain radix-2 FFT, in place.
struct Fft {
    n: usize,
    cos: Vec<f64>,
    sin: Vec<f64>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        let (cos, sin) = (0..n / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
                (a.cos(), a.sin())
            })
            .unzip();
        Fft { n, cos, sin }
    }

    fn run(&self, re: &mut [f64], im: &mut [f64]) {
        let n = self.n;
        // Bit-reversed order
        let mut j = 0;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = (self.cos[k * step], self.sin[k * step]);
                    let (a, b) = (start + k, start + k + len / 2);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_matches_a_plain_dft() {
        let n = 16;
        let fft = Fft::new(n);
        let x: Vec<f64> = (0..n).map(|i| ((i * 7) % 5) as f64 - 2.0).collect();
        let (mut re, mut im) = (x.clone(), vec![0.0; n]);
        fft.run(&mut re, &mut im);
        for k in 0..n {
            let (mut r, mut i) = (0.0, 0.0);
            for (t, v) in x.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * (k * t) as f64 / n as f64;
                r += v * a.cos();
                i += v * a.sin();
            }
            assert!(
                (re[k] - r).abs() < 1e-9 && (im[k] - i).abs() < 1e-9,
                "bin {k}"
            );
        }
    }

    #[test]
    fn filterbanks_peak_at_a_tone() {
        let fb = Fbank::new();
        let tone: Vec<i16> = (0..RATE)
            .map(|i| {
                (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / RATE as f64).sin())
                    as i16
            })
            .collect();
        let feats = fb.compute(&tone);
        assert_eq!(feats.len(), 1 + (RATE - FRAME) / SHIFT);
        let row = feats[10];
        let peak = (0..MELS)
            .max_by(|a, b| row[*a].total_cmp(&row[*b]))
            .unwrap();
        // 1 kHz is mel 1000, about a third of the way up to 8 kHz (mel 2840): band 27.
        assert!((26..=28).contains(&peak), "peak band {peak}");
    }

    /// Same voice scores higher than another voice. Needs the model and two voices recorded with
    /// macOS `say`: set BB_VOICE_MODEL and BB_VOICE_DIR (a.wav, a2.wav: one voice; b.wav: another).
    #[test]
    #[ignore]
    fn same_voice_is_closer() {
        let model = std::env::var("BB_VOICE_MODEL").unwrap();
        let dir = PathBuf::from(std::env::var("BB_VOICE_DIR").unwrap());
        let tmp = std::env::temp_dir().join(format!("bbvoice-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::copy(&model, tmp.join(MODEL.name)).unwrap();
        let engine = Engine::load(&tmp).unwrap();
        let read = |f: &str| -> Vec<i16> {
            hound::WavReader::open(dir.join(f))
                .unwrap()
                .into_samples::<i16>()
                .map(|s| s.unwrap())
                .collect()
        };
        let a = engine.embed(&read("a.wav")).unwrap();
        let a2 = engine.embed(&read("a2.wav")).unwrap();
        let b = engine.embed(&read("b.wav")).unwrap();
        let same = cosine(&a, &a2);
        let other = cosine(&a, &b);
        println!("same voice {same:.3}, other voice {other:.3}");
        assert!(same > other + 0.1, "same {same}, other {other}");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
