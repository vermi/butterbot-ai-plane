//! The camera on the handheld controller's screen in AI mode (the joystick pushed up on its home
//! screen): while it is open, the latest frame goes to the robot as a small JPEG with a caption of
//! what is in view, and the robot passes it on over Bluetooth. Closes by itself after a while.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use bbproto::ctrl::ToRobot;
use image::imageops::FilterType;

use crate::control::Robot;

/// The controller's picture: its 128 px wide screen, below the top bar.
const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;
/// About 2 KB a picture (five Bluetooth notifications): a few a second next to Wi-Fi.
const QUALITY: u8 = 40;
/// Look for a new camera frame this often.
const INTERVAL: Duration = Duration::from_millis(300);
/// Nobody looks this long; the controller closes it too.
const MAX_OPEN: Duration = Duration::from_secs(120);
const MAX_CAPTION: usize = 64;

/// Whether the controller is showing the camera. Each start gets a new generation; a stop (or a
/// newer start) ends the stream with an older one.
#[derive(Default)]
pub struct Peek {
    generation: AtomicU64,
}

impl Peek {
    pub fn start(&self, robot: Robot) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!("controller camera on");
        tokio::spawn(stream(robot, generation));
    }

    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        tracing::info!("controller camera off");
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Relaxed) == generation
    }
}

async fn stream(robot: Robot, generation: u64) {
    let started = Instant::now();
    let mut last_seq = None;
    while robot.peek.current(generation) && started.elapsed() < MAX_OPEN {
        if !robot.hub.is_connected().await || robot.mode().await.as_deref() != Some("remote") {
            break;
        }
        if let Some(frame) = robot.hub.latest_frame().await
            && last_seq != Some(frame.header.seq)
        {
            last_seq = Some(frame.header.seq);
            let jpeg = frame.jpeg.clone();
            match tokio::task::spawn_blocking(move || thumbnail(&jpeg)).await {
                Ok(Ok(small)) => {
                    let msg = ToRobot::Peek {
                        jpeg: base64::engine::general_purpose::STANDARD.encode(small),
                        caption: caption(&robot),
                    };
                    if let Err(e) = robot.hub.send(msg).await {
                        tracing::debug!("controller camera: {e:#}");
                    }
                }
                Ok(Err(e)) => tracing::debug!("controller camera: {e:#}"),
                Err(e) => tracing::warn!("controller camera: {e}"),
            }
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// `jpeg` shrunk to the controller's picture size.
fn thumbnail(jpeg: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .context("decoding the camera frame")?;
    let small = img
        .resize_exact(WIDTH, HEIGHT, FilterType::Triangle)
        .to_rgb8();
    let mut out = Vec::with_capacity(4096);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, QUALITY)
        .encode_image(&small)
        .context("encoding the thumbnail")?;
    Ok(out)
}

/// Who and what is in view, like "Justin, dog, couch".
fn caption(robot: &Robot) -> String {
    let mut parts: Vec<String> = Vec::new();
    let faces = robot.faces.in_view();
    for f in &faces {
        let name = f.name.clone().unwrap_or_else(|| "someone".into());
        if !parts.contains(&name) {
            parts.push(name);
        }
    }
    for kind in robot.objects.kinds_in_view() {
        if kind == "person" && !faces.is_empty() {
            continue;
        }
        if !parts.iter().any(|p| p == kind) {
            parts.push(kind.to_string());
        }
    }
    let mut out = String::new();
    for p in parts {
        let next = if out.is_empty() {
            p
        } else {
            format!("{out}, {p}")
        };
        if next.len() > MAX_CAPTION {
            break;
        }
        out = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_fit_the_screen_and_the_link() {
        let frame = image::RgbImage::from_fn(640, 480, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
            .encode_image(&frame)
            .unwrap();
        let small = thumbnail(&jpeg).unwrap();
        let back = image::load_from_memory(&small).unwrap();
        assert_eq!((back.width(), back.height()), (WIDTH, HEIGHT));
        assert!(small.len() < 8000, "{} bytes", small.len());
    }
}
