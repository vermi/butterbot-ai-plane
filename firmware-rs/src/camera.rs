//! The OV3660 camera, encoding JPEG itself (the sensor's own encoder: no CPU work here). Frames
//! go to the plane as `Video` frames: a live stream at the plane's size and rate, newest frame
//! wins when the network is slower, and snapshots on request.
//!
//! The driver is set up once for the largest size asked for so far (its frame buffers are sized
//! then) and switches sizes on the sensor after that. It is shut down when nothing has asked for
//! frames for a while.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bbproto::ctrl::Res;
use esp_idf_svc::sys::{self, camera as cam};

use crate::audio::{Phrase, Speaker};
use crate::link::Link;
use crate::pins;
use crate::util::{self, millis, Stack};

/// Off for this long: the driver stops (it captures continuously while up).
const IDLE_OFF_MS: u64 = 15_000;
/// Board default; above 128x128 the sensor's RGB output needed at most 20 MHz.
const DEFAULT_XCLK_MHZ: u8 = 20;
const STATS_MS: u64 = 10_000;

#[derive(Clone, Copy, PartialEq)]
struct Stream {
    res: Res,
    fps: u8,
    quality: u8,
    xclk: u8,
    psram_dma: bool,
}

struct Snapshot {
    id: u32,
    res: Res,
    quality: u8,
}

#[derive(Default)]
struct Want {
    stream: Option<Stream>,
    snapshot: Option<Snapshot>,
}

pub struct Camera {
    want: Mutex<Want>,
    changed: Condvar,
}

fn framesize(r: Res) -> cam::framesize_t {
    match r {
        Res::R128 => cam::framesize_t_FRAMESIZE_128X128,
        Res::Qvga => cam::framesize_t_FRAMESIZE_QVGA,
        Res::Vga => cam::framesize_t_FRAMESIZE_VGA,
        Res::Svga => cam::framesize_t_FRAMESIZE_SVGA,
        Res::Xga => cam::framesize_t_FRAMESIZE_XGA,
        Res::Hd => cam::framesize_t_FRAMESIZE_HD,
        Res::Uxga => cam::framesize_t_FRAMESIZE_UXGA,
        Res::Qxga => cam::framesize_t_FRAMESIZE_QXGA,
    }
}

fn pixels(r: Res) -> u32 {
    let (w, h) = r.size();
    w as u32 * h as u32
}

/// What the driver runs with.
struct Driver {
    /// The size its buffers were made for.
    max: Res,
    xclk: u8,
    psram_dma: bool,
    size: Res,
    quality: u8,
}

impl Driver {
    fn start(max: Res, xclk: u8, psram_dma: bool, quality: u8) -> Option<Driver> {
        let mut c: cam::camera_config_t = unsafe { core::mem::zeroed() };
        c.pin_pwdn = pins::CAM_PWDN;
        c.pin_reset = -1;
        c.pin_xclk = pins::CAM_XCLK;
        c.__bindgen_anon_1.pin_sccb_sda = -1;
        c.__bindgen_anon_2.pin_sccb_scl = -1;
        c.pin_d7 = pins::CAM_D7;
        c.pin_d6 = pins::CAM_D6;
        c.pin_d5 = pins::CAM_D5;
        c.pin_d4 = pins::CAM_D4;
        c.pin_d3 = pins::CAM_D3;
        c.pin_d2 = pins::CAM_D2;
        c.pin_d1 = pins::CAM_D1;
        c.pin_d0 = pins::CAM_D0;
        c.pin_vsync = pins::CAM_VSYNC;
        c.pin_href = pins::CAM_HREF;
        c.pin_pclk = pins::CAM_PCLK;
        c.xclk_freq_hz = xclk as i32 * 1_000_000;
        c.ledc_timer = sys::ledc_timer_t_LEDC_TIMER_0;
        c.ledc_channel = sys::ledc_channel_t_LEDC_CHANNEL_0;
        c.pixel_format = cam::pixformat_t_PIXFORMAT_JPEG;
        c.frame_size = framesize(max);
        c.jpeg_quality = quality as i32;
        c.fb_count = 2;
        c.fb_location = cam::camera_fb_location_t_CAMERA_FB_IN_PSRAM;
        c.grab_mode = cam::camera_grab_mode_t_CAMERA_GRAB_LATEST;
        c.sccb_i2c_port = 0;
        let t = millis();
        // DMA straight into PSRAM finds a JPEG's end only in the last DMA block and drops
        // nearly every frame (NO-EOI); through internal buffers it sees the whole frame.
        unsafe { cam::esp_camera_set_psram_mode(psram_dma) };
        let err = unsafe { cam::esp_camera_init(&c) };
        if err != sys::ESP_OK {
            log::error!("camera: init failed ({err:#x})");
            return None;
        }
        unsafe {
            let s = cam::esp_camera_sensor_get();
            if !s.is_null() {
                if let Some(f) = (*s).set_vflip {
                    f(s, 1);
                }
                if let Some(f) = (*s).set_hmirror {
                    f(s, 0);
                }
            }
        }
        // The first frames after init are often dark or partial.
        for _ in 0..2 {
            let fb = unsafe { cam::esp_camera_fb_get() };
            if !fb.is_null() {
                unsafe { cam::esp_camera_fb_return(fb) };
            }
        }
        log::info!(
            "camera: up for {:?} at {xclk} MHz{} in {} ms",
            max,
            if psram_dma { ", PSRAM DMA" } else { "" },
            millis() - t
        );
        Some(Driver {
            max,
            xclk,
            psram_dma,
            size: max,
            quality,
        })
    }

    fn stop(self) {
        unsafe { cam::esp_camera_deinit() };
        log::info!("camera: off");
    }

    fn set(&mut self, size: Res, quality: u8) {
        unsafe {
            let s = cam::esp_camera_sensor_get();
            if s.is_null() {
                return;
            }
            if size != self.size {
                if let Some(f) = (*s).set_framesize {
                    f(s, framesize(size));
                }
                self.size = size;
                // The first frames after a change are often partial or badly exposed.
                for _ in 0..2 {
                    let fb = cam::esp_camera_fb_get();
                    if !fb.is_null() {
                        cam::esp_camera_fb_return(fb);
                    }
                }
            }
            if quality != self.quality {
                if let Some(f) = (*s).set_quality {
                    f(s, quality as i32);
                }
                self.quality = quality;
            }
        }
    }

    /// The newest frame, copied out.
    fn grab(&self) -> Option<(Vec<u8>, u16, u16)> {
        unsafe {
            let fb = cam::esp_camera_fb_get();
            if fb.is_null() {
                return None;
            }
            let f = &*fb;
            let out = core::slice::from_raw_parts(f.buf, f.len).to_vec();
            let (w, h) = (f.width as u16, f.height as u16);
            cam::esp_camera_fb_return(fb);
            Some((out, w, h))
        }
    }
}

impl Camera {
    pub fn start(link: Arc<Link>, speaker: Option<Arc<Speaker>>) -> Arc<Camera> {
        let c = Arc::new(Camera {
            want: Mutex::new(Want::default()),
            changed: Condvar::new(),
        });
        let me = c.clone();
        util::spawn(c"camera", 6144, 6, Stack::Psram, move || {
            me.run(&link, speaker.as_deref())
        });
        c
    }

    pub fn set_stream(
        &self,
        on: bool,
        res: Res,
        fps: u8,
        quality: u8,
        xclk: Option<u8>,
        psram_dma: Option<bool>,
    ) {
        let mut w = self.want.lock().unwrap();
        w.stream = on.then(|| Stream {
            res,
            fps: fps.clamp(1, 30),
            quality: quality.clamp(4, 63),
            xclk: xclk
                .filter(|&x| (8..=40).contains(&x))
                .unwrap_or(DEFAULT_XCLK_MHZ),
            psram_dma: psram_dma.unwrap_or(false),
        });
        self.changed.notify_one();
    }

    /// Answered with an ack and the frame (or an error) by the camera thread.
    pub fn snapshot(&self, link: &Link, id: u32, res: Res, quality: u8) {
        let mut w = self.want.lock().unwrap();
        if let Some(old) = w.snapshot.take() {
            link.error(old.id, "superseded by a newer snapshot");
        }
        w.snapshot = Some(Snapshot {
            id,
            res,
            quality: quality.clamp(4, 63),
        });
        self.changed.notify_one();
    }

    pub fn stop(&self) {
        let mut w = self.want.lock().unwrap();
        w.stream = None;
        self.changed.notify_one();
    }

    fn run(&self, link: &Link, speaker: Option<&Speaker>) {
        let mut failure_said = false;
        let mut driver: Option<Driver> = None;
        let mut idle_since = millis();
        let mut next_frame = 0u64;
        let mut seq = 0u32;
        let (mut frames, mut bytes, mut stats_at) = (0u32, 0usize, millis());
        loop {
            let (stream, snapshot) = {
                let mut w = self.want.lock().unwrap();
                if w.stream.is_none() && w.snapshot.is_none() {
                    let wait = if driver.is_some() { 1000 } else { 60_000 };
                    w = self
                        .changed
                        .wait_timeout(w, Duration::from_millis(wait))
                        .unwrap()
                        .0;
                }
                (w.stream, w.snapshot.take())
            };
            let now = millis();
            if stream.is_none() && snapshot.is_none() {
                if driver.is_some() && now - idle_since > IDLE_OFF_MS {
                    driver.take().map(Driver::stop);
                }
                continue;
            }
            idle_since = now;

            // The size and clock the driver needs for what is asked now.
            let mut need = stream.map(|s| s.res).unwrap_or(Res::R128);
            if let Some(s) = &snapshot {
                if pixels(s.res) > pixels(need) {
                    need = s.res;
                }
            }
            let xclk = stream.map(|s| s.xclk).unwrap_or(DEFAULT_XCLK_MHZ);
            let psram_dma = stream.is_some_and(|s| s.psram_dma);
            let quality = stream.map(|s| s.quality).unwrap_or(12);
            if driver.as_ref().is_some_and(|d| {
                pixels(need) > pixels(d.max) || d.xclk != xclk || d.psram_dma != psram_dma
            }) {
                driver.take().map(Driver::stop);
            }
            if driver.is_none() {
                driver = Driver::start(need, xclk, psram_dma, quality);
                if driver.is_none() {
                    // Once: it would keep saying it otherwise.
                    if !failure_said {
                        failure_said = true;
                        if let Some(s) = speaker {
                            s.say(Phrase::CameraFailure);
                        }
                    }
                    if let Some(s) = snapshot {
                        link.error(s.id, "camera init failed");
                    }
                    util::sleep_ms(2000);
                    continue;
                }
            }
            let d = driver.as_mut().unwrap();

            if let Some(s) = snapshot {
                d.set(s.res, s.quality);
                match d.grab() {
                    Some((jpeg, w, h)) => {
                        link.ack(s.id, serde_json::Value::Null);
                        link.send_video(jpeg, w, h, s.id, true);
                    }
                    None => link.error(s.id, "no frame from camera"),
                }
                continue;
            }
            let Some(st) = stream else {
                continue;
            };
            d.set(st.res, st.quality);
            let now = millis();
            if now < next_frame {
                util::sleep_ms((next_frame - now).min(50));
                continue;
            }
            // Network slower than the camera: skip rather than queue stale frames.
            if link.video_pending() {
                util::sleep_ms(2);
                continue;
            }
            next_frame = now + 1000 / st.fps as u64;
            if let Some((jpeg, w, h)) = d.grab() {
                seq = seq.wrapping_add(1);
                frames += 1;
                bytes += jpeg.len();
                link.send_video(jpeg, w, h, seq, false);
            }
            if millis() - stats_at >= STATS_MS {
                let secs = (millis() - stats_at) as f32 / 1000.0;
                log::info!(
                    "camera: {:.1} fps at {:?} q{}, {} KB/frame",
                    frames as f32 / secs,
                    st.res,
                    st.quality,
                    bytes / frames.max(1) as usize / 1024
                );
                (frames, bytes, stats_at) = (0, 0, millis());
            }
        }
    }
}
