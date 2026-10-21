//! The camera on the handheld controller's screen in AI mode (the joystick pushed up on its home
//! screen).
//!
//! Bluetooth is far too slow for video, so the controller joins the home Wi-Fi (the robot gives it
//! the details) and says hello here every second. While it does, each new camera frame goes to it
//! shrunk to its screen, in the RC feed's format (`FeedFrame` in ButterBot-Common) so its RC video
//! code shows it, and what is in view goes through the robot as a caption. Meanwhile the camera
//! runs at QVGA: more frames a second, at more than the controller can show anyway.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bbproto::ctrl::ToRobot;
use image::imageops::FilterType;
use tokio::net::UdpSocket;

use crate::control::Robot;

/// `PeekWiFiData::PlanePort`: the controller says hello here.
pub const PORT: u16 = 7533;
/// `UDPPort` on the controller: its feed listens here.
const FEED_PORT: u16 = 6001;
const HELLO: &[u8] = b"BBPEEK";
/// No hello for this long: the view is closed.
const GONE: Duration = Duration::from_secs(3);
/// The controller's picture: its 128 px wide screen, below the top bar.
const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;
const QUALITY: u8 = 50;
/// Neither end reassembles fragmented datagrams: a frame goes in pieces of this size, which the
/// controller's feed puts back together from its stream.
const DATAGRAM: usize = 1400;
const MAX_CAPTION: usize = 64;
/// `FeedFrame::Header`, `Trailer` and `SizeShift`.
const HEADER: [u8; 8] = [0x18, 0x20, 0x55, 0xf2, 0x5a, 0xc0, 0x4d, 0xaa];
const TRAILER: [u8; 8] = [0x42, 0x2c, 0xd9, 0xe3, 0xff, 0xa0, 0x11, 0x01];
const SIZE_SHIFT: [usize; 4] = [2, 3, 1, 0];

/// Serve the controller's camera view. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let sock = match UdpSocket::bind(("0.0.0.0", PORT)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("controller camera view unavailable (udp {PORT}): {e}");
            return;
        }
    };
    let mut viewer: Option<(SocketAddr, Instant)> = None;
    let mut last_seq = None;
    let mut caption_sent = (String::new(), Instant::now());
    let mut sent = 0u64;
    let mut buf = [0u8; 64];
    let mut tick = tokio::time::interval(Duration::from_millis(40));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                if &buf[..n] != HELLO {
                    continue;
                }
                if viewer.is_none() {
                    tracing::info!("controller camera view open ({})", from.ip());
                    robot.camera_fast(true).await;
                    caption_sent.0.clear();
                }
                viewer = Some((SocketAddr::new(from.ip(), FEED_PORT), Instant::now()));
            }
            _ = tick.tick() => {
                let Some((to, at)) = viewer else { continue };
                if at.elapsed() > GONE {
                    tracing::info!("controller camera view closed");
                    viewer = None;
                    robot.camera_fast(false).await;
                    continue;
                }
                if let Some(frame) = robot.hub.latest_frame().await
                    && last_seq != Some(frame.header.seq)
                {
                    last_seq = Some(frame.header.seq);
                    let jpeg = frame.jpeg.clone();
                    match tokio::task::spawn_blocking(move || thumbnail(&jpeg)).await {
                        Ok(Ok(small)) => {
                            match send_frame(&sock, to, &small).await {
                                Ok(()) => {
                                    sent += 1;
                                    if sent % 20 == 1 {
                                        tracing::info!(
                                            "controller camera view: {sent} frames sent to {to} ({} bytes each)",
                                            small.len()
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!("controller camera view: {e:#}"),
                            }
                        }
                        Ok(Err(e)) => tracing::debug!("controller camera view: {e:#}"),
                        Err(e) => tracing::warn!("controller camera view: {e}"),
                    }
                }
                let text = caption(&robot);
                if (text != caption_sent.0 || caption_sent.1.elapsed() > Duration::from_secs(5))
                    && robot.hub.send(ToRobot::PeekCaption { text: text.clone() }).await.is_ok()
                {
                    caption_sent = (text, Instant::now());
                }
            }
        }
    }
}

/// One frame in the RC feed's framing: header, size, size again byte-shifted, JPEG, trailer.
fn framed(jpeg: &[u8]) -> Vec<u8> {
    let size = (jpeg.len() as u32).to_le_bytes();
    let mut shifted = [0u8; 4];
    for i in 0..4 {
        shifted[SIZE_SHIFT[i]] = size[i];
    }
    let mut out = Vec::with_capacity(jpeg.len() + 24);
    out.extend_from_slice(&HEADER);
    out.extend_from_slice(&size);
    out.extend_from_slice(&shifted);
    out.extend_from_slice(jpeg);
    out.extend_from_slice(&TRAILER);
    out
}

async fn send_frame(sock: &UdpSocket, to: SocketAddr, jpeg: &[u8]) -> Result<()> {
    for piece in framed(jpeg).chunks(DATAGRAM) {
        sock.send_to(piece, to).await.context("sending a frame")?;
    }
    Ok(())
}

/// `jpeg` shrunk to the controller's picture size.
fn thumbnail(jpeg: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .context("decoding the camera frame")?;
    let small = img
        .resize_exact(WIDTH, HEIGHT, FilterType::Triangle)
        .to_rgb8();
    let mut out = Vec::with_capacity(6000);
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
    fn thumbnails_fit_the_screen() {
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
        // The controller's feed takes at most 8000 bytes a frame.
        assert!(small.len() < 8000, "{} bytes", small.len());
    }

    #[test]
    fn framing_matches_the_rc_feed() {
        let f = framed(&[1, 2, 3, 4, 5]);
        assert_eq!(&f[..8], &HEADER);
        assert_eq!(&f[8..12], &[5, 0, 0, 0]);
        // SizeShift {2, 3, 1, 0}: byte i of the size goes to position SizeShift[i]
        assert_eq!(&f[12..16], &[0, 0, 5, 0]);
        assert_eq!(&f[16..21], &[1, 2, 3, 4, 5]);
        assert_eq!(&f[21..], &TRAILER);
    }
}
