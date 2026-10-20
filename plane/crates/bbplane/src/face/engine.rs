//! Face detection (YuNet) and face embeddings (SFace), run with tract on the laptop's CPU.
//!
//! Both models come from the OpenCV model zoo and are downloaded once into the data directory,
//! checked against a pinned SHA-256. The pre- and post-processing follows OpenCV's
//! `FaceDetectorYN` and `FaceRecognizerSF`, which these models were trained for.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use image::RgbImage;
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

struct ModelFile {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
}

/// YuNet face detector (MIT license, Shiqi Yu).
const DETECTOR: ModelFile = ModelFile {
    name: "face_detection_yunet_2023mar.onnx",
    url: "https://github.com/opencv/opencv_zoo/raw/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx",
    sha256: "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4",
};

/// SFace face recognizer (Apache-2.0).
const RECOGNIZER: ModelFile = ModelFile {
    name: "face_recognition_sface_2021dec.onnx",
    url: "https://github.com/opencv/opencv_zoo/raw/main/models/face_recognition_sface/face_recognition_sface_2021dec.onnx",
    sha256: "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79",
};

/// The detector's fixed input size.
const DET_SIZE: u32 = 640;
const STRIDES: [u32; 3] = [8, 16, 32];
const DET_SCORE: f32 = 0.7;
const NMS_IOU: f32 = 0.3;
/// SFace input size.
const REC_SIZE: u32 = 112;
pub const EMBEDDING_LEN: usize = 128;

/// Where the five landmarks (right eye, left eye, nose tip, right and left mouth corner) sit in
/// SFace's aligned 112x112 input.
const TEMPLATE: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

type Plan = Arc<TypedRunnableModel>;

/// A face found in an image, in that image's pixel coordinates.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Detection {
    pub score: f32,
    /// Top-left corner, width and height.
    pub bbox: [f32; 4],
    pub landmarks: [[f32; 2]; 5],
}

pub struct Engine {
    detector: Plan,
    recognizer: Plan,
}

impl Engine {
    /// Load the models from `dir`, downloading any that are missing.
    pub fn load(dir: &Path) -> Result<Engine> {
        let det = fetch(dir, &DETECTOR)?;
        let rec = fetch(dir, &RECOGNIZER)?;
        Ok(Engine {
            detector: load_plan(&det)?,
            recognizer: load_plan(&rec)?,
        })
    }

    /// Faces in `img`, best first.
    pub fn detect(&self, img: &RgbImage) -> Result<Vec<Detection>> {
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        // Scale to fit the fixed 640x640 input (small robot frames get enlarged, which helps
        // with small faces) and pad the rest with black.
        let scale = (DET_SIZE as f32 / w as f32).min(DET_SIZE as f32 / h as f32);
        let (sw, sh) = (
            ((w as f32 * scale) as u32).clamp(1, DET_SIZE),
            ((h as f32 * scale) as u32).clamp(1, DET_SIZE),
        );
        let scaled = image::imageops::resize(img, sw, sh, image::imageops::FilterType::Triangle);
        let n = DET_SIZE as usize;
        // NCHW, BGR, raw 0..255 values.
        let input = tract_ndarray::Array4::from_shape_fn((1, 3, n, n), |(_, c, y, x)| {
            if x < sw as usize && y < sh as usize {
                scaled.get_pixel(x as u32, y as u32)[2 - c] as f32
            } else {
                0.0
            }
        });
        let out = self.detector.run(tvec!(Tensor::from(input).into()))?;
        if out.len() != 12 {
            bail!("unexpected detector outputs: {}", out.len());
        }
        let mut dets = Vec::new();
        for (i, &stride) in STRIDES.iter().enumerate() {
            let cls = floats(&out[i])?;
            let obj = floats(&out[3 + i])?;
            let bbox = floats(&out[6 + i])?;
            let kps = floats(&out[9 + i])?;
            decode(stride, cls, obj, bbox, kps, &mut dets);
        }
        let mut dets = nms(dets, NMS_IOU);
        for d in &mut dets {
            for v in &mut d.bbox {
                *v /= scale;
            }
            for p in &mut d.landmarks {
                p[0] /= scale;
                p[1] /= scale;
            }
        }
        Ok(dets)
    }

    /// Unit-length embedding of the face `det` in `img`. Embeddings of the same person have a
    /// high dot product.
    pub fn embed(&self, img: &RgbImage, det: &Detection) -> Result<Vec<f32>> {
        let aligned = align(img, &det.landmarks);
        let n = REC_SIZE as usize;
        // NCHW, RGB, raw 0..255 values.
        let input = tract_ndarray::Array4::from_shape_fn((1, 3, n, n), |(_, c, y, x)| {
            aligned.get_pixel(x as u32, y as u32)[c] as f32
        });
        let out = self.recognizer.run(tvec!(Tensor::from(input).into()))?;
        let v = floats(&out[0])?.to_vec();
        if v.len() != EMBEDDING_LEN {
            bail!("unexpected embedding length {}", v.len());
        }
        Ok(normalize(v))
    }
}

fn floats(t: &Tensor) -> Result<&[f32]> {
    t.try_as_plain_ram()?.as_slice::<f32>()
}

fn load_plan(path: &Path) -> Result<Plan> {
    tract_onnx::onnx()
        .model_for_path(path)
        .and_then(|m| m.into_optimized())
        .and_then(|m| m.into_runnable())
        .with_context(|| format!("loading {}", path.display()))
}

/// Path of `m` in `dir`, downloading and verifying it first if needed.
fn fetch(dir: &Path, m: &ModelFile) -> Result<PathBuf> {
    let path = dir.join(m.name);
    if let Ok(bytes) = std::fs::read(&path) {
        if sha256_hex(&bytes) == m.sha256 {
            return Ok(path);
        }
        tracing::warn!("{} is damaged; downloading it again", path.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    tracing::info!("downloading {}", m.url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .into();
    let mut bytes = Vec::new();
    agent
        .get(m.url)
        .call()
        .with_context(|| format!("downloading {}", m.url))?
        .into_body()
        .into_reader()
        .take(100 << 20)
        .read_to_end(&mut bytes)
        .with_context(|| format!("downloading {}", m.url))?;
    let got = sha256_hex(&bytes);
    if got != m.sha256 {
        bail!(
            "{} has SHA-256 {got}, expected {}; not using it",
            m.url,
            m.sha256
        );
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Turn one stride's raw outputs into detections in 640x640 input coordinates.
fn decode(
    stride: u32,
    cls: &[f32],
    obj: &[f32],
    bbox: &[f32],
    kps: &[f32],
    out: &mut Vec<Detection>,
) {
    let cols = (DET_SIZE / stride) as usize;
    let s = stride as f32;
    for idx in 0..cls.len().min(obj.len()) {
        let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
        if score < DET_SCORE || bbox.len() < (idx + 1) * 4 || kps.len() < (idx + 1) * 10 {
            continue;
        }
        let (r, c) = ((idx / cols) as f32, (idx % cols) as f32);
        let b = &bbox[idx * 4..idx * 4 + 4];
        let (cx, cy) = ((c + b[0]) * s, (r + b[1]) * s);
        let (w, h) = (b[2].exp() * s, b[3].exp() * s);
        let k = &kps[idx * 10..idx * 10 + 10];
        let mut landmarks = [[0.0; 2]; 5];
        for (i, p) in landmarks.iter_mut().enumerate() {
            *p = [(k[2 * i] + c) * s, (k[2 * i + 1] + r) * s];
        }
        out.push(Detection {
            score,
            bbox: [cx - w / 2.0, cy - h / 2.0, w, h],
            landmarks,
        });
    }
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let x1 = a[0].max(b[0]);
    let y1 = a[1].max(b[1]);
    let x2 = (a[0] + a[2]).min(b[0] + b[2]);
    let y2 = (a[1] + a[3]).min(b[1] + b[3]);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Greedy non-maximum suppression; returns the kept detections, best first.
fn nms(mut dets: Vec<Detection>, threshold: f32) -> Vec<Detection> {
    dets.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    for d in dets {
        if kept.iter().all(|k| iou(&k.bbox, &d.bbox) <= threshold) {
            kept.push(d);
        }
    }
    kept
}

/// Least-squares similarity transform (rotation, uniform scale, translation) taking `src` onto
/// `dst`, as `[a, b, tx, ty]` for `x' = a*x - b*y + tx`, `y' = b*x + a*y + ty`.
fn similarity(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [f32; 4] {
    let mean = |p: &[[f32; 2]; 5]| {
        let (sx, sy) = p.iter().fold((0.0, 0.0), |(x, y), q| (x + q[0], y + q[1]));
        [sx / 5.0, sy / 5.0]
    };
    let (ms, md) = (mean(src), mean(dst));
    let (mut num_a, mut num_b, mut den) = (0.0, 0.0, 0.0);
    for (s, d) in src.iter().zip(dst) {
        let (px, py) = (s[0] - ms[0], s[1] - ms[1]);
        let (qx, qy) = (d[0] - md[0], d[1] - md[1]);
        num_a += px * qx + py * qy;
        num_b += px * qy - py * qx;
        den += px * px + py * py;
    }
    if den <= f32::EPSILON {
        return [1.0, 0.0, md[0] - ms[0], md[1] - ms[1]];
    }
    let (a, b) = (num_a / den, num_b / den);
    [
        a,
        b,
        md[0] - (a * ms[0] - b * ms[1]),
        md[1] - (b * ms[0] + a * ms[1]),
    ]
}

/// The 112x112 face crop SFace expects: landmarks moved onto `TEMPLATE`, bilinear sampling,
/// black outside the source image.
fn align(img: &RgbImage, landmarks: &[[f32; 2]; 5]) -> RgbImage {
    let [a, b, tx, ty] = similarity(landmarks, &TEMPLATE);
    let det = a * a + b * b;
    let (w, h) = img.dimensions();
    RgbImage::from_fn(REC_SIZE, REC_SIZE, |u, v| {
        if det <= f32::EPSILON {
            return image::Rgb([0, 0, 0]);
        }
        // Invert x' = M x + t.
        let (dx, dy) = (u as f32 - tx, v as f32 - ty);
        let sx = (a * dx + b * dy) / det;
        let sy = (-b * dx + a * dy) / det;
        bilinear(img, w, h, sx, sy)
    })
}

fn bilinear(img: &RgbImage, w: u32, h: u32, x: f32, y: f32) -> image::Rgb<u8> {
    if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
        return image::Rgb([0, 0, 0]);
    }
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let p = |x, y| img.get_pixel(x, y).0;
    let (p00, p10, p01, p11) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    let mut out = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bottom = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = (top * (1.0 - fy) + bottom * fy).round() as u8;
    }
    image::Rgb(out)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// Cosine similarity of two unit-length embeddings.
pub fn similarity_score(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_recovers_a_known_transform() {
        // Rotate 30 degrees, scale 1.5, shift (10, -4).
        let (sin, cos) = 30f32.to_radians().sin_cos();
        let (a, b) = (1.5 * cos, 1.5 * sin);
        let src = [[0.0, 0.0], [10.0, 0.0], [5.0, 5.0], [2.0, 9.0], [8.0, 9.0]];
        let dst = src.map(|[x, y]| [a * x - b * y + 10.0, b * x + a * y - 4.0]);
        let m = similarity(&src, &dst);
        for (got, want) in m.iter().zip([a, b, 10.0, -4.0]) {
            assert!((got - want).abs() < 1e-4, "{m:?}");
        }
    }

    #[test]
    fn align_maps_landmarks_onto_the_template() {
        // A face whose landmarks are exactly the template, shifted and doubled in size.
        let lm = TEMPLATE.map(|[x, y]| [x * 2.0 + 30.0, y * 2.0 + 20.0]);
        let mut img = RgbImage::new(300, 300);
        // Mark the nose tip.
        let [nx, ny] = lm[2];
        for y in ny as u32 - 2..=ny as u32 + 2 {
            for x in nx as u32 - 2..=nx as u32 + 2 {
                img.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
        let out = align(&img, &lm);
        let [tx, ty] = TEMPLATE[2];
        assert_eq!(out.get_pixel(tx.round() as u32, ty.round() as u32)[0], 255);
        assert_eq!(out.get_pixel(5, 5)[0], 0);
    }

    #[test]
    fn decode_and_nms() {
        // One strong anchor at row 2, col 3 of the stride-32 grid, plus a weaker duplicate.
        let n = (640 / 32) * (640 / 32);
        let (mut cls, mut obj) = (vec![0.0; n], vec![0.0; n]);
        let (mut bbox, kps) = (vec![0.0; n * 4], vec![0.5; n * 10]);
        let i = 2 * 20 + 3;
        cls[i] = 0.95;
        obj[i] = 0.95;
        cls[i + 1] = 0.8;
        obj[i + 1] = 0.8;
        bbox[(i + 1) * 4] = -1.0; // same centre as anchor i
        let mut dets = Vec::new();
        decode(32, &cls, &obj, &bbox, &kps, &mut dets);
        assert_eq!(dets.len(), 2);
        let dets = nms(dets, NMS_IOU);
        assert_eq!(dets.len(), 1);
        let d = &dets[0];
        assert!((d.score - 0.95).abs() < 1e-5);
        // Centre (3*32, 2*32), size 32 (exp(0) * stride).
        assert_eq!(d.bbox, [96.0 - 16.0, 64.0 - 16.0, 32.0, 32.0]);
        assert_eq!(d.landmarks[0], [(0.5 + 3.0) * 32.0, (0.5 + 2.0) * 32.0]);
    }

    #[test]
    fn embeddings_compare_by_cosine() {
        let a = normalize(vec![1.0, 2.0, 3.0]);
        let b = normalize(vec![2.0, 4.0, 6.0]);
        let c = normalize(vec![-3.0, 0.0, 1.0]);
        assert!((similarity_score(&a, &b) - 1.0).abs() < 1e-6);
        assert!(similarity_score(&a, &c).abs() < 1e-6);
    }
}
