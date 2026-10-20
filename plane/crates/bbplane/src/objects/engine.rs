//! Object detection with YOLOX-S (COCO's 80 everyday classes), run with tract on the laptop's CPU
//! in about 0.15 s a frame. The model comes from the YOLOX releases and is downloaded once into
//! the data directory, checked against a pinned SHA-256. Pre- and post-processing follow YOLOX's
//! own ONNX demo. (YOLOX-Tiny is 3x faster but misses a dog lying on the floor in the robot's
//! 320x240 view.)

use std::path::Path;

use anyhow::{Context, Result, bail};
use image::RgbImage;
use tract_onnx::prelude::*;

use crate::models::{ModelFile, fetch};

/// YOLOX-S (Apache-2.0, Megvii).
const DETECTOR: ModelFile = ModelFile {
    max_bytes: 48 << 20,
    name: "yolox_s.onnx",
    url: "https://github.com/Megvii-BaseDetection/YOLOX/releases/download/0.1.1rc0/yolox_s.onnx",
    sha256: "c5c2d13e59ae883e6af3b45daea64af4833a4951c92d116ec270d9ddbe998063",
};

/// The model's fixed input size.
const SIZE: u32 = 640;
const STRIDES: [u32; 3] = [8, 16, 32];
/// Per box: x, y, w, h, objectness, then one score per class.
const ROW: usize = 5 + CLASSES.len();
const NMS_IOU: f32 = 0.45;
/// Letterbox padding value YOLOX was trained with.
const PAD: f32 = 114.0;

pub const CLASSES: [&str; 80] = [
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

/// An object found in an image, in that image's pixel coordinates.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Detection {
    pub class: &'static str,
    pub score: f32,
    /// Top-left corner, width and height.
    pub bbox: [f32; 4],
}

pub struct Engine {
    detector: Arc<TypedRunnableModel>,
}

impl Engine {
    /// Load the model from `dir`, downloading it if it is missing.
    pub fn load(dir: &Path) -> Result<Engine> {
        let path = fetch(dir, &DETECTOR)?;
        let detector = tract_onnx::onnx()
            .model_for_path(&path)
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .with_context(|| format!("loading {}", path.display()))?;
        Ok(Engine { detector })
    }

    /// Objects in `img` scoring at least `min_score`, best first.
    pub fn detect(&self, img: &RgbImage, min_score: f32) -> Result<Vec<Detection>> {
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        // Scale to fit, top-left aligned, padded with grey.
        let scale = (SIZE as f32 / w as f32).min(SIZE as f32 / h as f32);
        let (sw, sh) = (
            ((w as f32 * scale) as u32).clamp(1, SIZE),
            ((h as f32 * scale) as u32).clamp(1, SIZE),
        );
        let scaled = image::imageops::resize(img, sw, sh, image::imageops::FilterType::Triangle);
        let n = SIZE as usize;
        // NCHW, BGR, raw 0..255 values.
        let input = tract_ndarray::Array4::from_shape_fn((1, 3, n, n), |(_, c, y, x)| {
            if x < sw as usize && y < sh as usize {
                scaled.get_pixel(x as u32, y as u32)[2 - c] as f32
            } else {
                PAD
            }
        });
        let out = self.detector.run(tvec!(Tensor::from(input).into()))?;
        let rows = out
            .first()
            .context("no detector output")?
            .try_as_plain_ram()?
            .as_slice::<f32>()?;
        let mut dets = decode(rows, min_score)?;
        dets = nms(dets, NMS_IOU);
        for d in &mut dets {
            for v in &mut d.bbox {
                *v /= scale;
            }
        }
        Ok(dets)
    }
}

/// Raw rows (one per grid cell, strides 8, 16 and 32 in turn) to detections in input pixels.
fn decode(rows: &[f32], min_score: f32) -> Result<Vec<Detection>> {
    let cells: usize = STRIDES
        .iter()
        .map(|s| ((SIZE / s) * (SIZE / s)) as usize)
        .sum();
    if rows.len() != cells * ROW {
        bail!(
            "unexpected detector output size {} (expected {})",
            rows.len(),
            cells * ROW
        );
    }
    let mut dets = Vec::new();
    let mut row = rows.as_chunks::<ROW>().0.iter();
    for stride in STRIDES {
        let cols = SIZE / stride;
        let s = stride as f32;
        for gy in 0..cols {
            for gx in 0..cols {
                let r = row.next().context("detector output too short")?;
                let (class, cls) = r[5..]
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .context("no classes")?;
                let score = r[4] * cls;
                if score < min_score {
                    continue;
                }
                let (cx, cy) = ((r[0] + gx as f32) * s, (r[1] + gy as f32) * s);
                let (w, h) = (r[2].exp() * s, r[3].exp() * s);
                dets.push(Detection {
                    class: CLASSES[class],
                    score,
                    bbox: [cx - w / 2.0, cy - h / 2.0, w, h],
                });
            }
        }
    }
    Ok(dets)
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

/// Greedy non-maximum suppression within each class; returns the kept detections, best first.
fn nms(mut dets: Vec<Detection>, threshold: f32) -> Vec<Detection> {
    dets.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    for d in dets {
        if kept
            .iter()
            .all(|k| k.class != d.class || iou(&k.bbox, &d.bbox) <= threshold)
        {
            kept.push(d);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_cell_and_suppresses_overlaps() {
        let cells: usize = STRIDES
            .iter()
            .map(|s| ((SIZE / s) * (SIZE / s)) as usize)
            .sum();
        let mut rows = vec![0.0; cells * ROW];
        // Stride 8, grid cell (2, 1): a confident cat, centred at ((2 + 0.5) * 8, (1 + 0.5) * 8),
        // 8 * e^1 wide.
        let cols = (SIZE / 8) as usize;
        let i = (cols + 2) * ROW;
        rows[i..i + 5].copy_from_slice(&[0.5, 0.5, 1.0, 0.0, 0.9]);
        rows[i + 5 + 15] = 0.8;
        // The next cell sees the same cat a bit less surely: suppressed.
        let j = (cols + 3) * ROW;
        rows[j..j + 5].copy_from_slice(&[-0.5, 0.5, 1.0, 0.0, 0.8]);
        rows[j + 5 + 15] = 0.8;
        let dets = nms(decode(&rows, 0.3).unwrap(), NMS_IOU);
        assert_eq!(dets.len(), 1);
        let d = &dets[0];
        assert_eq!(d.class, "cat");
        assert!((d.score - 0.72).abs() < 1e-5);
        let w = 8.0 * 1f32.exp();
        assert!((d.bbox[0] - (20.0 - w / 2.0)).abs() < 1e-3);
        assert!((d.bbox[1] - 8.0).abs() < 1e-3);
        assert!((d.bbox[2] - w).abs() < 1e-3);
    }

    #[test]
    fn rejects_unexpected_output() {
        assert!(decode(&[0.0; 85], 0.3).is_err());
    }

    /// Runs the real model on a camera frame: `BBPLANE_MODELS=<dir> BBPLANE_FRAME=<jpeg>`.
    #[test]
    #[ignore]
    fn detects_on_a_real_frame() {
        let dir = std::env::var("BBPLANE_MODELS").unwrap();
        let frame = std::env::var("BBPLANE_FRAME").unwrap();
        let engine = Engine::load(Path::new(&dir)).unwrap();
        let img = image::open(&frame).unwrap().to_rgb8();
        let t = std::time::Instant::now();
        let dets = engine.detect(&img, 0.2).unwrap();
        let first = t.elapsed();
        let t = std::time::Instant::now();
        engine.detect(&img, 0.3).unwrap();
        eprintln!("first {first:?}, then {:?}", t.elapsed());
        for d in &dets {
            eprintln!("{} {:.2} {:?}", d.class, d.score, d.bbox);
        }
    }
}
