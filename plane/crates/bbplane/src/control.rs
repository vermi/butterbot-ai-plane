//! High-level robot operations shared by the MCP server and the web UI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use bbproto::ctrl::{LedMode, LedTarget, Res};
use bbproto::frame::{FrameType, VideoFrame};
use bbproto::{Frame, ToRobot};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::audio::{self, RATE};
use crate::hub::Hub;
use crate::tts;

/// Watchdog per drive message; the plane refreshes it every DRIVE_REFRESH while driving.
const DRIVE_SLICE_MS: u64 = 600;
/// The robot's watchdog must outlast the plane's final Stop, or a late "watchdog" stop event
/// would cut short the next drive.
const DRIVE_TAIL_MS: u64 = 150;
const DRIVE_REFRESH: Duration = Duration::from_millis(250);
/// Hard cap for one drive/turn tool call.
pub const MAX_DRIVE_MS: u64 = 10_000;
/// Open-loop calibration: a full-speed spin in place turns about 180 degrees in this time
/// (stock firmware `WanderRoutine`/`VoiceTurn180Routine`).
pub const MS_PER_180_AT_FULL: f64 = 4000.0;
/// Longest text for one `say` (the robot's frame limit is far larger; this keeps speech sane).
pub const MAX_SAY_BYTES: usize = 2000;
/// Speaker chunk: 32 ms, one datagram.
const SPK_CHUNK: usize = 512;
/// How far ahead of real time speaker audio is sent.
const SPK_LEAD: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct Robot {
    pub hub: Arc<Hub>,
    pub faces: Arc<crate::face::Faces>,
    pub ears: Arc<crate::ears::Ears>,
    /// Serialises wake attempts: many tool calls / clicks share one BLE wake.
    wake_lock: Arc<Mutex<()>>,
    speaker: Arc<Mutex<Option<CancellationToken>>>,
    drive: Arc<Mutex<Option<CancellationToken>>>,
    /// Camera and mic state last asked for, restored when the link comes back.
    streams: Arc<Mutex<Streams>>,
    /// Set while the plane itself is waking the robot: the caller is about to use it, so
    /// `on_link_up` does not announce "Connected." over whatever that caller says.
    plane_woke: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Copy)]
struct Streams {
    camera_on: bool,
    res: Res,
    fps: u8,
    quality: u8,
    opts: bbproto::ctrl::CamOpts,
    mic_on: bool,
}

impl Streams {
    fn defaults(cfg: &crate::config::Config) -> Streams {
        Streams {
            camera_on: true,
            res: cfg.camera.res,
            fps: cfg.camera.fps,
            quality: cfg.camera.quality,
            opts: cfg.camera.opts,
            mic_on: true,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DriveOutcome {
    pub completed_ms: u64,
    pub stopped_early: Option<String>,
}

impl Robot {
    pub fn new(hub: Arc<Hub>) -> Robot {
        Robot {
            faces: Arc::new(crate::face::Faces::new(hub.clone())),
            ears: Arc::new(crate::ears::Ears::new(hub.clone())),
            streams: Arc::new(Mutex::new(Streams::defaults(&hub.config))),
            hub,
            wake_lock: Default::default(),
            speaker: Default::default(),
            drive: Default::default(),
            plane_woke: Default::default(),
        }
    }

    pub async fn mode(&self) -> Option<String> {
        self.hub.status.lock().await.mode.clone()
    }

    /// Enter remote mode if needed (and allowed by config), then start the configured streams.
    pub async fn ensure_remote(&self) -> Result<()> {
        if self.hub.is_connected().await && self.mode().await.as_deref() == Some("remote") {
            return Ok(());
        }
        if !self.hub.config.auto_enter_remote {
            bail!("the robot is not in AI mode; call enter_ai_mode first");
        }
        self.enter_remote().await
    }

    /// Bring the robot into AI mode. The robot only runs Wi-Fi while in AI mode, so when it is
    /// not connected it is woken over Bluetooth first; it then dials in already in remote mode.
    pub async fn enter_remote(&self) -> Result<()> {
        if !self.hub.is_connected().await {
            let _guard = self.wake_lock.lock().await;
            if !self.hub.is_connected().await {
                self.reconnect_or_wake().await?;
            }
        }
        if self.mode().await.as_deref() != Some("remote") {
            // Take the cursor before asking, so an old "remote" event cannot satisfy the wait.
            let since = self.hub.last_event_seq().await;
            let id = self.hub.next_id();
            self.hub
                .request_timeout(ToRobot::EnterRemote { id }, Duration::from_secs(10))
                .await?;
            if self.mode().await.as_deref() != Some("remote") {
                let ok = self
                    .hub
                    .wait_event(since, Duration::from_secs(5), |e| {
                        e.kind == "mode" && e.data["mode"] == "remote"
                    })
                    .await;
                if ok.is_none() && self.mode().await.as_deref() != Some("remote") {
                    bail!(
                        "robot did not enter remote mode (it may be busy in a scenario or RC mode)"
                    );
                }
            }
        }
        // Entering AI mode on purpose starts from the configured streams.
        *self.streams.lock().await = Streams::defaults(&self.hub.config);
        self.apply_session_settings().await
    }

    /// Start the camera and microphone streams as last asked for (the configured ones unless
    /// changed since) and set the safety checks. Each AI mode session on the robot starts with
    /// all of them off, and a re-link must not undo a "camera off".
    pub async fn apply_session_settings(&self) -> Result<()> {
        let cfg = &self.hub.config;
        let s = *self.streams.lock().await;
        self.set_camera(s.camera_on, s.res, s.fps, s.quality, s.opts)
            .await?;
        self.set_mic(s.mic_on).await?;
        let id = self.hub.next_id();
        let safety = &cfg.safety;
        self.hub
            .request(ToRobot::Safety {
                id,
                tilt: safety.tilt,
                obstacle: safety.obstacle,
            })
            .await?;
        Ok(())
    }

    /// A robot that dropped out of remote mode keeps retrying for ~15-20 s by itself; give it that
    /// time before using Bluetooth (BLE traffic competes with Wi-Fi recovery on the robot's radio).
    async fn reconnect_or_wake(&self) -> Result<()> {
        const SELF_RECONNECT: Duration = Duration::from_secs(20);
        let lost = *self.hub.remote_lost_at.lock().await;
        if let Some(lost) = lost {
            let until = lost + SELF_RECONNECT;
            while tokio::time::Instant::now() < until {
                if self.hub.is_connected().await {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        *self.hub.remote_lost_at.lock().await = None;
        self.wake().await
    }

    /// Wake the robot over Bluetooth and wait until it has dialed in and reported remote mode.
    pub async fn wake(&self) -> Result<()> {
        if !self.hub.config.ble_wake {
            bail!("the robot is not connected; wake it with `bbplane wake` (ble_wake is off)");
        }
        let since = self.hub.last_event_seq().await;
        tracing::info!("waking the robot over Bluetooth");
        self.plane_woke
            .store(true, std::sync::atomic::Ordering::Relaxed);
        crate::provision::wake_isolated().await.map_err(|e| {
            self.plane_woke
                .store(false, std::sync::atomic::Ordering::Relaxed);
            anyhow::anyhow!("the robot is not connected and could not be woken over Bluetooth: {e}")
        })?;
        // Joining Wi-Fi and DHCP usually take 3-10 s.
        let ev = self
            .hub
            .wait_event(since, Duration::from_secs(45), |e| {
                e.kind == "mode" && e.data["mode"] == "remote"
            })
            .await;
        if ev.is_none() {
            bail!(
                "the robot was woken but did not connect within 45 s; check that it is on the same \
                 2.4 GHz network and can reach this machine on port {}",
                self.hub.config.link_port
            );
        }
        Ok(())
    }

    pub async fn exit_remote(&self) -> Result<()> {
        self.stop().await.ok();
        let id = self.hub.next_id();
        self.hub.request(ToRobot::ExitRemote { id }).await?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        if let Some(t) = self.drive.lock().await.take() {
            t.cancel();
        }
        self.hub.send(ToRobot::Stop).await
    }

    /// Drive with raw motor speeds for `ms`, refreshing the robot-side watchdog.
    /// Returns early if the robot reports that it stopped on its own (obstacle, lifted, ...).
    pub async fn drive_for(&self, l: i8, r: i8, ms: u64) -> Result<DriveOutcome> {
        self.ensure_remote().await?;
        let ms = ms.min(MAX_DRIVE_MS);
        let token = CancellationToken::new();
        if let Some(old) = self.drive.lock().await.replace(token.clone()) {
            old.cancel();
        }
        let since = self.hub.last_event_seq().await;
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_millis(ms);
        let mut stopped_early = None;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let left = (deadline - now).as_millis() as u64;
            self.hub
                .send(ToRobot::Drive {
                    l,
                    r,
                    ms: (left + DRIVE_TAIL_MS).min(DRIVE_SLICE_MS) as u16,
                    safe: true,
                })
                .await?;
            let tick = tokio::time::sleep(DRIVE_REFRESH.min(deadline - now));
            let stopped = self
                .hub
                .wait_event(since, DRIVE_REFRESH.min(deadline - now), |e| {
                    e.kind == "drive_stopped"
                });
            tokio::select! {
                _ = token.cancelled() => { stopped_early = Some("cancelled".into()); break; }
                ev = stopped => if let Some(ev) = ev {
                    stopped_early = Some(ev.data["reason"].as_str().unwrap_or("robot stopped").to_string());
                    break;
                },
                _ = tick => {}
            }
        }
        if stopped_early.as_deref() != Some("cancelled") {
            self.hub.send(ToRobot::Stop).await.ok();
        }
        Ok(DriveOutcome {
            completed_ms: start.elapsed().as_millis() as u64,
            stopped_early,
        })
    }

    /// Spin in place by roughly `degrees` (positive = clockwise/right). Open loop.
    pub async fn turn(&self, degrees: f64, speed: u8) -> Result<DriveOutcome> {
        let speed = speed.clamp(30, 100) as f64;
        let ms = (degrees.abs() / 180.0 * MS_PER_180_AT_FULL * (100.0 / speed)) as u64;
        let s = speed as i8;
        let (l, r) = if degrees >= 0.0 { (s, -s) } else { (-s, s) };
        self.drive_for(l, r, ms).await
    }

    pub async fn set_camera(
        &self,
        on: bool,
        res: Res,
        fps: u8,
        quality: u8,
        opts: bbproto::ctrl::CamOpts,
    ) -> Result<()> {
        let id = self.hub.next_id();
        self.hub
            .request(ToRobot::Camera {
                id,
                on,
                res,
                fps: fps.clamp(1, 30),
                quality: quality.clamp(4, 63),
                opts,
            })
            .await?;
        self.hub.status.lock().await.camera_streaming = on;
        let mut s = self.streams.lock().await;
        s.camera_on = on;
        if on {
            (s.res, s.fps, s.quality, s.opts) = (res, fps, quality, opts);
        }
        Ok(())
    }

    pub async fn set_mic(&self, on: bool) -> Result<()> {
        let id = self.hub.next_id();
        let m = &self.hub.config.mic;
        self.hub
            .request(ToRobot::Mic {
                id,
                on,
                gate: m.gate,
                gain: m.gain,
            })
            .await?;
        self.hub.status.lock().await.mic_streaming = on;
        self.streams.lock().await.mic_on = on;
        Ok(())
    }

    pub async fn snapshot(&self, res: Option<Res>, quality: u8) -> Result<VideoFrame> {
        self.ensure_remote().await?;
        match res {
            Some(res) => self.hub.snapshot(res, quality).await,
            None => {
                // Newest live frame, waiting briefly for one if the stream just started.
                let mut rx = self.hub.video.subscribe();
                if let Some(f) = self.hub.latest_frame().await {
                    return Ok((*f).clone());
                }
                match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                    Ok(Ok(f)) => Ok((*f).clone()),
                    _ => self.hub.snapshot(self.hub.config.camera.res, quality).await,
                }
            }
        }
    }

    /// Record `seconds` of microphone audio: the next seconds, or the last ones if `past`.
    pub async fn listen(&self, seconds: f32, past: bool) -> Result<Vec<i16>> {
        let seconds = seconds.clamp(0.5, 30.0);
        if past {
            return Ok(self.hub.mic_history(seconds).await);
        }
        self.ensure_remote().await?;
        if !self.hub.status.lock().await.mic_streaming {
            self.set_mic(true).await?;
        }
        let want = (seconds * RATE as f32) as usize;
        let mut rx = self.hub.mic.subscribe();
        let mut out = Vec::with_capacity(want);
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f32(seconds + 3.0);
        while out.len() < want {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(chunk)) => out.extend_from_slice(&chunk),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => bail!("robot disconnected while listening"),
                Err(_) => {
                    if out.is_empty() {
                        bail!("no microphone audio arrived from the robot");
                    }
                    break;
                }
            }
        }
        out.truncate(want);
        Ok(out)
    }

    /// Speak `text`. `engine` overrides the configured engine (`robot` or `command`).
    pub async fn say(&self, text: &str, engine: Option<&str>, wait: bool) -> Result<String> {
        self.ensure_remote().await?;
        if text.len() > MAX_SAY_BYTES {
            bail!(
                "text too long ({} bytes, max {MAX_SAY_BYTES}); split it into several calls",
                text.len()
            );
        }
        let engine = engine.unwrap_or(&self.hub.config.tts.engine).to_string();
        match engine.as_str() {
            "robot" => {
                let id = self.hub.next_id();
                let since = self.hub.last_event_seq().await;
                self.hub
                    .request(ToRobot::Say {
                        id,
                        text: text.to_string(),
                    })
                    .await?;
                if wait {
                    self.wait_speaker_idle(since, text.len()).await;
                }
                Ok("robot".into())
            }
            "command" => {
                let pcm = tts::synthesize(&self.hub.config.tts, text).await?;
                self.play_pcm(pcm, wait).await?;
                Ok("command".into())
            }
            other => bail!("unknown TTS engine '{other}' (use 'robot' or 'command')"),
        }
    }

    async fn wait_speaker_idle(&self, since: u64, text_len: usize) {
        // flite speaks roughly 15 characters per second; allow generous slack.
        let limit = Duration::from_secs_f32(3.0 + text_len as f32 / 8.0);
        let started = self
            .hub
            .wait_event(since, Duration::from_secs(3), |e| {
                e.kind == "speaker" && e.data["playing"] == true
            })
            .await;
        let since = started.map(|e| e.seq).unwrap_or(since);
        self.hub
            .wait_event(since, limit, |e| {
                e.kind == "speaker" && e.data["playing"] == false
            })
            .await;
    }

    /// Stream 16 kHz mono PCM to the robot speaker, paced to real time.
    pub async fn play_pcm(&self, pcm: Vec<i16>, wait: bool) -> Result<()> {
        self.ensure_remote().await?;
        let token = CancellationToken::new();
        if let Some(old) = self.speaker.lock().await.replace(token.clone()) {
            old.cancel();
            self.hub.send(ToRobot::SpkAbort).await.ok();
        }
        let id = self.hub.next_id();
        self.hub.request(ToRobot::SpkBegin { id }).await?;
        let hub = self.hub.clone();
        let task = tokio::spawn(async move { stream_pcm(hub, pcm, token).await });
        if wait {
            task.await??;
        }
        Ok(())
    }

    pub async fn stop_audio(&self) -> Result<()> {
        if let Some(t) = self.speaker.lock().await.take() {
            t.cancel();
        }
        self.hub.send(ToRobot::SpkAbort).await
    }

    pub async fn set_volume(&self, gain: f32) -> Result<()> {
        self.ensure_remote().await?;
        self.hub
            .send(ToRobot::Volume {
                gain: gain.clamp(0.0, 1.0),
            })
            .await
    }

    pub async fn led(&self, target: LedTarget, mode: LedMode, level: f32) -> Result<Value> {
        self.ensure_remote().await?;
        let id = self.hub.next_id();
        self.hub
            .request(ToRobot::Led {
                id,
                target,
                mode,
                level: level.clamp(0.0, 1.0),
            })
            .await
    }

    pub async fn scenario(&self, name: &str, arg: u8) -> Result<Value> {
        if bbproto::bb::NEEDS_MODELS.contains(&name) {
            bail!("'{name}' needs on-robot speech/vision models and is not available in AI mode");
        }
        let Some(scenario) = bbproto::bb::scenario_id(name) else {
            bail!(
                "unknown scenario '{name}'; known: {}",
                bbproto::bb::SCENARIOS.join(", ")
            );
        };
        self.ensure_remote().await?;
        let id = self.hub.next_id();
        // Scenarios run to completion on the robot before it acks.
        let r = self
            .hub
            .request_timeout(
                ToRobot::Scenario { id, scenario, arg },
                Duration::from_secs(90),
            )
            .await;
        if r.is_err() {
            // Do not leave the robot stuck in a routine nobody is waiting for.
            self.hub.send(ToRobot::Stop).await.ok();
        }
        r
    }

    pub async fn ir_list(&self) -> Result<Value> {
        self.ensure_remote().await?;
        let id = self.hub.next_id();
        self.hub.request(ToRobot::IrList { id }).await
    }

    pub async fn ir_send(&self, index: u8) -> Result<Value> {
        self.ensure_remote().await?;
        let id = self.hub.next_id();
        self.hub.request(ToRobot::IrSend { id, index }).await
    }

    pub async fn modules(&self) -> Result<Value> {
        self.ensure_remote().await?;
        let id = self.hub.next_id();
        self.hub.request(ToRobot::Modules { id }).await
    }

    pub async fn status_json(&self) -> Value {
        let s = self.hub.status.lock().await.clone();
        let latest = self
            .hub
            .latest_frame()
            .await
            .map(|f| json!({ "width": f.width, "height": f.height, "seq": f.header.seq }));
        json!({
            "robot": s,
            "latest_frame": latest,
            "faces": self.faces.status_json(),
            "last_event_seq": self.hub.last_event_seq().await,
        })
    }
}

/// When the robot links up in AI mode, whoever started it (its button, `bbplane wake`, the AI
/// or a restarted plane), start its streams and say "Connected." so a person who pressed the
/// button knows it is ready. Spawned once by `crate::start`.
pub async fn on_link_up(robot: Robot) {
    let mut cursor = robot.hub.last_event_seq().await;
    let mut linked = false;
    loop {
        for e in robot
            .hub
            .events_since(cursor, Duration::from_secs(60))
            .await
        {
            cursor = e.seq;
            match e.kind.as_str() {
                "link" => linked = e.data["connected"] == true,
                "mode" if linked && e.data["mode"] == "remote" => {
                    linked = false;
                    let robot = robot.clone();
                    let woken_by_plane = robot
                        .plane_woke
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    tokio::spawn(async move {
                        if let Err(e) = robot.apply_session_settings().await {
                            tracing::warn!("starting the robot's streams: {e:#}");
                        }
                        if robot.hub.config.announce_connect
                            && !woken_by_plane
                            && let Err(e) = robot.say("Connected.", Some("robot"), false).await
                        {
                            tracing::debug!("connect announcement: {e:#}");
                        }
                    });
                }
                _ => {}
            }
        }
    }
}

async fn stream_pcm(hub: Arc<Hub>, pcm: Vec<i16>, token: CancellationToken) -> Result<()> {
    let start = tokio::time::Instant::now();
    let mut sent = 0usize;
    for chunk in pcm.chunks(SPK_CHUNK) {
        if token.is_cancelled() {
            return Ok(());
        }
        // Keep at most SPK_LEAD of audio ahead of real time.
        let ahead = Duration::from_secs_f64(sent as f64 / RATE as f64);
        let due = start + ahead.saturating_sub(SPK_LEAD);
        tokio::select! {
            _ = token.cancelled() => return Ok(()),
            _ = tokio::time::sleep_until(due) => {}
        }
        hub.send_frame(Frame::new(
            FrameType::SpkPcm,
            bbproto::frame::pcm_to_le(chunk),
        ))
        .await?;
        sent += chunk.len();
    }
    hub.send(ToRobot::SpkEnd).await?;
    // Let the robot play out the buffered lead before reporting done.
    let total = Duration::from_secs_f64(sent as f64 / RATE as f64);
    tokio::select! {
        _ = token.cancelled() => {}
        _ = tokio::time::sleep_until(start + total + Duration::from_millis(100)) => {}
    }
    Ok(())
}

/// Encode a level-stats + WAV pair for tools that return audio.
pub fn clip_summary(samples: &[i16]) -> (Vec<u8>, audio::LevelStats) {
    (audio::wav_bytes(samples, RATE), audio::level_stats(samples))
}
