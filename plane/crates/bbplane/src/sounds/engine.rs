//! Sound recognition with YAMNet (Google, Apache-2.0; AudioSet's 521 classes), run with tract on
//! the laptop's CPU. The ONNX conversion (tf2onnx of the original SavedModel, weights unchanged,
//! with the log-mel front end inside) and its class names are downloaded once into the data
//! directory and checked against pinned SHA-256 hashes.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tract_onnx::prelude::*;

use crate::models::{ModelFile, fetch};

const MODEL: ModelFile = ModelFile {
    max_bytes: 24 << 20,
    name: "yamnet.onnx",
    url: "https://huggingface.co/audiomagic/yamnet-onnx/resolve/main/yamnet.onnx",
    sha256: "d3835ffbbd4a1bb3e777f0ca217b5007907f5171dd5d17c4236b95b2af8f908e",
};

/// AudioSet class names (the ontology is CC BY 4.0).
const CLASSES: ModelFile = ModelFile {
    max_bytes: 64 << 10,
    name: "yamnet_class_map.csv",
    url: "https://huggingface.co/audiomagic/yamnet-onnx/resolve/main/yamnet_class_map.csv",
    sha256: "cdf24d193e196d9e95912a2667051ae203e92a2ba09449218ccb40ef787c6df2",
};

pub const RATE: u32 = 16_000;
/// One analysis window: YAMNet's 0.96 s patch.
pub const WINDOW: usize = 15_360;

pub struct Engine {
    plan: Arc<TypedRunnableModel>,
    classes: Vec<String>,
}

impl Engine {
    pub fn load(dir: &Path) -> Result<Engine> {
        let model = fetch(dir, &MODEL)?;
        let classes = parse_classes(&std::fs::read_to_string(fetch(dir, &CLASSES)?)?)?;
        let plan = tract_onnx::onnx()
            .model_for_path(&model)
            .and_then(|m| m.with_input_fact(0, f32::fact([WINDOW]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .with_context(|| format!("loading {}", model.display()))?;
        Ok(Engine { plan, classes })
    }

    /// Scores (0..1) of every class for one window of 16 kHz audio in -1..1, best first.
    pub fn classify(&self, window: &[f32]) -> Result<Vec<(&str, f32)>> {
        if window.len() != WINDOW {
            bail!("expected {WINDOW} samples, got {}", window.len());
        }
        let input = tract_ndarray::Array1::from_vec(window.to_vec());
        let out = self.plan.run(tvec!(Tensor::from(input).into()))?;
        let scores = out
            .first()
            .context("no output")?
            .try_as_plain_ram()?
            .as_slice::<f32>()?;
        let n = self.classes.len();
        if scores.len() < n {
            bail!("unexpected output size {}", scores.len());
        }
        // One window gives one frame; take the first row.
        let mut ranked: Vec<(&str, f32)> = self
            .classes
            .iter()
            .map(String::as_str)
            .zip(scores[..n].iter().copied())
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok(ranked)
    }

    pub fn classes(&self) -> &[String] {
        &self.classes
    }
}

/// `index,mid,display_name` rows; names may be quoted and contain commas.
fn parse_classes(csv: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for line in csv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(3, ',');
        let (Some(_), Some(_), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("bad class map line: {line}");
        };
        names.push(name.trim().trim_matches('"').to_string());
    }
    if names.len() != 521 {
        bail!("expected 521 classes, found {}", names.len());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_names() {
        let mut csv = String::from("index,mid,display_name\n");
        for i in 0..521 {
            if i == 1 {
                csv.push_str("1,/m/0ytgt,\"Child speech, kid speaking\"\n");
            } else {
                csv.push_str(&format!("{i},/m/x,Class {i}\n"));
            }
        }
        let names = parse_classes(&csv).unwrap();
        assert_eq!(names[1], "Child speech, kid speaking");
        assert_eq!(names[2], "Class 2");
    }

    /// Runs the real model: `BBPLANE_MODELS=<dir> [BBPLANE_WAV=<16 kHz mono wav>]`.
    #[test]
    #[ignore]
    fn classifies_real_audio() {
        let dir = std::env::var("BBPLANE_MODELS").unwrap();
        let engine = Engine::load(Path::new(&dir)).unwrap();
        let beep: Vec<f32> = (0..WINDOW)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE as f32).sin())
            .collect();
        let t = std::time::Instant::now();
        let top = engine.classify(&beep).unwrap();
        eprintln!("1 kHz tone ({:?}): {:?}", t.elapsed(), &top[..3]);
        if let Ok(wav) = std::env::var("BBPLANE_WAV") {
            let bytes = std::fs::read(wav).unwrap();
            let pcm: Vec<f32> = bytes[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
                .collect();
            for w in pcm.as_chunks::<WINDOW>().0.iter().take(4) {
                eprintln!("wav: {:?}", &engine.classify(w).unwrap()[..3]);
            }
        }
    }
}
