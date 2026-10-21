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
/// Anyone heard speaking this recently counts as company for idle nudges.
const COMPANY: Duration = Duration::from_secs(180);
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
    pub objects: Arc<crate::objects::Objects>,
    pub sounds: Arc<crate::sounds::Sounds>,
    pub reminders: Arc<crate::reminders::Reminders>,
    pub memory: Arc<crate::memory::Memory>,
    pub ears: Arc<crate::ears::Ears>,
    pub life: Arc<crate::life::Life>,
    pub agent: Arc<crate::agent::AgentState>,
    pub light: Arc<crate::light::Light>,
    /// What it is up to right now and until when, for the controller's screen (see `doing`).
    doing: Arc<std::sync::Mutex<Option<(String, std::time::Instant)>>>,
    /// The camera settings to go back to after the controller's camera view (see `camera_fast`).
    camera_before_view: Arc<Mutex<Option<(Res, u8, u8, bbproto::ctrl::CamOpts)>>>,
    /// How it feels (see `crate::mood`).
    pub mood: Arc<crate::mood::Mood>,
    /// Its own plans (see `crate::intentions`).
    pub intentions: Arc<crate::intentions::Intentions>,
    /// Its diary (see `crate::diary`).
    pub diary: Arc<crate::diary::Diary>,
    /// The Apple TV (see `crate::appletv`).
    pub appletv: Arc<crate::appletv::AppleTv>,
    /// Who is talking, by voice (see `crate::voices`).
    pub voices: Arc<crate::voices::Voices>,
    /// Serialises wake attempts: many tool calls / clicks share one BLE wake.
    wake_lock: Arc<Mutex<()>>,
    speaker: Arc<Mutex<Option<CancellationToken>>>,
    /// Bumped each time someone cuts the robot off (the controller's Shut up): speech queued
    /// before that is dropped.
    hushes: Arc<std::sync::atomic::AtomicU64>,
    /// Someone drives with the controller's joystick (Manual Override): the agent does not.
    pub manual_override: Arc<std::sync::atomic::AtomicBool>,
    /// A Summon in progress (the controller's button), to cancel.
    summon: Arc<Mutex<Option<CancellationToken>>>,
    drive: Arc<Mutex<Option<CancellationToken>>>,
    /// The robot is driving (or just stopped) until then: the jolts are its own, not someone
    /// handling it.
    moving_until: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
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
            objects: Arc::new(crate::objects::Objects::new(hub.clone())),
            sounds: Arc::new(crate::sounds::Sounds::new(&hub)),
            reminders: Arc::new(crate::reminders::Reminders::load(&hub.config.data_dir())),
            memory: Arc::new(crate::memory::Memory::load(&hub.config.data_dir())),
            mood: Arc::new(crate::mood::Mood::load(&hub.config.data_dir())),
            intentions: Arc::new(crate::intentions::Intentions::load(&hub.config.data_dir())),
            diary: Arc::new(crate::diary::Diary::load(&hub.config.data_dir())),
            appletv: Arc::new(crate::appletv::AppleTv::new(&hub.config.data_dir())),
            voices: Arc::new(crate::voices::Voices::load(&hub.config.data_dir())),
            ears: Arc::new(crate::ears::Ears::new(hub.clone())),
            life: Arc::new(crate::life::Life::new(hub.config.life.clone())),
            agent: Default::default(),
            light: Default::default(),
            camera_before_view: Default::default(),
            doing: Default::default(),
            streams: Arc::new(Mutex::new(Streams::defaults(&hub.config))),
            hub,
            wake_lock: Default::default(),
            speaker: Default::default(),
            drive: Default::default(),
            moving_until: Default::default(),
            hushes: Default::default(),
            manual_override: Default::default(),
            summon: Default::default(),
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
    /// Plugged in, and not allowed to drive on a cable (`[safety] drive_when_plugged`).
    pub async fn tethered(&self) -> bool {
        !self.hub.config.safety.drive_when_plugged
            && self
                .hub
                .status
                .lock()
                .await
                .battery
                .as_ref()
                .and_then(|b| b["charging"].as_str().map(|c| c != "unplugged"))
                .unwrap_or(false)
    }

    pub async fn drive_for(&self, l: i8, r: i8, ms: u64) -> Result<DriveOutcome> {
        self.ensure_remote().await?;
        if self
            .manual_override
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            bail!("someone is driving the robot with the remote (Manual Override) right now");
        }
        {
            if self.tethered().await {
                bail!(
                    "the robot is plugged in (USB or charger); it does not drive on a cable. \
Ask to be unplugged first."
                );
            }
        }
        let ms = ms.min(MAX_DRIVE_MS);
        let token = CancellationToken::new();
        if let Some(old) = self.drive.lock().await.replace(token.clone()) {
            old.cancel();
        }
        let since = self.hub.last_event_seq().await;
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_millis(ms);
        self.moving_for(Duration::from_millis(ms));
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
        self.moving_for(Duration::ZERO);
        Ok(DriveOutcome {
            completed_ms: start.elapsed().as_millis() as u64,
            stopped_early,
        })
    }

    /// Note that the robot drives for `d` (plus a moment to settle).
    pub fn moving_for(&self, d: Duration) {
        let until = std::time::Instant::now() + d + Duration::from_millis(1500);
        let mut m = self.moving_until.lock().unwrap();
        if m.is_none_or(|t| t < until) {
            *m = Some(until);
        }
    }

    pub fn is_moving(&self) -> bool {
        self.moving_until
            .lock()
            .unwrap()
            .is_some_and(|t| std::time::Instant::now() < t)
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

    /// Say what it is up to for `d` ("dancing"): shown on the controller's screen.
    pub fn set_doing(&self, what: &str, d: Duration) {
        *self.doing.lock().unwrap() = Some((what.to_string(), std::time::Instant::now() + d));
    }

    pub fn doing(&self) -> Option<String> {
        let mut doing = self.doing.lock().unwrap();
        match doing.as_ref() {
            Some((what, until)) if std::time::Instant::now() < *until => Some(what.clone()),
            _ => {
                *doing = None;
                None
            }
        }
    }

    /// The controller's camera view wants frames more than detail: QVGA runs at about three
    /// times VGA's rate. `false` goes back to what was set before.
    pub async fn camera_fast(&self, fast: bool) {
        let mut before = self.camera_before_view.lock().await;
        let r = if fast {
            let s = self.streams.lock().await;
            if !s.camera_on || s.res == Res::Qvga || before.is_some() {
                return;
            }
            let (res, fps, quality, opts) = (s.res, s.fps, s.quality, s.opts);
            drop(s);
            *before = Some((res, fps, quality, opts));
            self.set_camera(true, Res::Qvga, fps, quality, opts).await
        } else {
            let Some((res, fps, quality, opts)) = before.take() else {
                return;
            };
            self.set_camera(true, res, fps, quality, opts).await
        };
        if let Err(e) = r {
            tracing::debug!("camera for the controller's view: {e:#}");
        }
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
        let r = self.say_inner(text, engine, wait).await;
        if r.is_ok() {
            self.life.activity();
            self.hub
                .push_event("said", serde_json::json!({ "text": text }))
                .await;
        }
        r
    }

    /// Record that the robot said `text` (spoken some other way than `say`, such as a built-in
    /// brain's streamed voice).
    pub async fn note_said(&self, text: &str) {
        self.life.activity();
        self.hub
            .push_event("said", serde_json::json!({ "text": text }))
            .await;
    }

    /// Speak in the robot's own voice and wait, without recording it (the caller records the
    /// whole reply with `note_said`).
    pub async fn say_quietly(&self, text: &str) -> Result<()> {
        self.say_inner(text, Some("robot"), true).await.map(|_| ())
    }

    /// Stay quiet (no nudges, no follow-up listening) for `d`, until called by name.
    /// The activity level last chosen (kept in `<data_dir>/activity.json`), applied at start.
    pub fn load_activity(&self) {
        let path = self.hub.config.data_dir().join("activity.json");
        if let Some(a) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| {
                v["activity"]
                    .as_str()
                    .and_then(crate::life::Activity::parse)
            })
        {
            self.life.set_activity_level(a);
        }
    }

    /// How much the robot does of its own accord, kept across restarts.
    pub fn set_activity(&self, a: crate::life::Activity) {
        self.life.set_activity_level(a);
        let dir = self.hub.config.data_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(
            dir.join("activity.json"),
            json!({ "activity": a.name() }).to_string(),
        ) {
            tracing::warn!("saving the activity level: {e}");
        }
    }

    /// Wander a little of its own accord: turn somewhere at random and drive a short hop.
    pub async fn wander(&self) -> Result<String> {
        let mut rng = crate::hub::now_ms() | 1;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let degrees = (30 + next() % 120) as f64 * if next() % 2 == 0 { 1.0 } else { -1.0 };
        self.set_doing("wandering", Duration::from_secs(4));
        self.turn(degrees, 55).await?;
        let ms = 500 + next() % 600;
        let out = self.drive_for(50, 50, ms).await?;
        Ok(match out.stopped_early {
            Some(why) => format!("Turned {degrees:.0} degrees, then stopped ({why})."),
            None => format!(
                "Turned {degrees:.0} degrees and drove forward for {:.1} s.",
                ms as f64 / 1000.0
            ),
        })
    }

    pub fn focus(&self, d: Duration) {
        self.life.focus(d);
        self.ears.set_attentive(false);
    }

    pub fn unfocus(&self) {
        self.life.unfocus();
        self.ears.set_attentive(true);
    }

    /// Run the stock shutdown routine: the robot says so and switches itself off. Refused unless
    /// the last thing said to it asked for that.
    pub async fn power_off(&self) -> Result<()> {
        if !self.ears.asked_to_power_off() {
            bail!(
                "not switching off: nobody asked you to turn off (\"goodbye\" just means \
goodbye). Say bye and keep listening."
            );
        }
        self.ensure_remote().await?;
        let scenario = bbproto::bb::scenario_id("shutdown")
            .ok_or_else(|| anyhow::anyhow!("no shutdown scenario"))?;
        let id = self.hub.next_id();
        // The robot loses power before it can acknowledge; do not wait for an answer.
        self.hub
            .send(ToRobot::Scenario {
                id,
                scenario,
                arg: 0,
            })
            .await
    }

    async fn say_inner(&self, text: &str, engine: Option<&str>, wait: bool) -> Result<String> {
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

    /// Come to whoever pressed Summon (see `crate::summon`), replacing one in progress.
    pub async fn start_summon(&self) {
        let token = CancellationToken::new();
        if let Some(old) = self.summon.lock().await.replace(token.clone()) {
            old.cancel();
        }
        let robot = self.clone();
        tokio::spawn(async move {
            robot.life.activity();
            if let Err(e) = robot.say("Coming!", Some("robot"), true).await {
                tracing::debug!("summon: {e:#}");
            }
            let said = match crate::summon::run(&robot, &token).await {
                Ok(s) => s,
                Err(e) if format!("{e:#}").contains("plugged in") => {
                    "I'm plugged in, so I can't come to you."
                }
                Err(e) => {
                    tracing::warn!("summon: {e:#}");
                    "I can't come right now."
                }
            };
            if token.is_cancelled() || said.is_empty() {
                return;
            }
            // They called it over: what they say next is for it.
            robot.ears.conversation_opened();
            if let Err(e) = robot.say(said, Some("robot"), false).await {
                tracing::debug!("summon: {e:#}");
            }
        });
    }

    pub async fn cancel_summon(&self) {
        if let Some(t) = self.summon.lock().await.take() {
            t.cancel();
            let _ = self.stop().await;
        }
    }

    pub fn hush_count(&self) -> u64 {
        self.hushes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cut the robot off: whatever it was about to say is dropped, and the conversation ends.
    pub async fn hush(&self) {
        self.hushes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.mood.appraise(
            crate::mood::Emotion::Sad,
            0.25,
            "I was told to stop talking",
        );
        if let Err(e) = self.stop_audio().await {
            tracing::debug!("hush: {e:#}");
        }
        self.ears.end_conversation();
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

    /// The phone link: `status` (connection, notification count, what is playing),
    /// `notifications` (all of them), or a music control: `play`, `pause`, `next`, `previous`.
    pub async fn phone(&self, action: &str) -> Result<Value> {
        self.ensure_remote().await?;
        if action != "notifications" {
            let id = self.hub.next_id();
            return self
                .hub
                .request(ToRobot::Phone {
                    id,
                    action: action.into(),
                    index: 0,
                })
                .await;
        }
        // A reply fits one datagram, so they come a page at a time.
        let mut all = Vec::new();
        let mut index = 0;
        loop {
            let id = self.hub.next_id();
            let page = self
                .hub
                .request(ToRobot::Phone {
                    id,
                    action: action.into(),
                    index,
                })
                .await?;
            all.extend(
                page["notifications"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            match page["next"].as_u64() {
                Some(n) if n > index as u64 && n < 256 && all.len() < 64 => index = n as u8,
                _ => return Ok(json!({ "connected": page["connected"], "notifications": all })),
            }
        }
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
            "objects": self.objects.status_json(),
            "sounds": self.sounds.status_json(),
            "focus_minutes_left": self.life.focused().map(|d| d.as_secs().div_ceil(60)),
            "activity": self.life.activity_level().name(),
            "agent": {
                "enabled": self.hub.config.agent.enabled,
                "running": self.life.agent_running.load(std::sync::atomic::Ordering::Relaxed),
                "paused": self.agent.paused.load(std::sync::atomic::Ordering::Relaxed),
            },
            "last_event_seq": self.hub.last_event_seq().await,
            "ai_state": (s.mode.as_deref() == Some("remote"))
                .then(|| format!("{:?}", crate::light::state(self)).to_lowercase()),
            "mood": self.mood.for_agent(),
            "doing": self.doing(),
        })
    }
}

/// Something for the agent to respond to (see `Robot::next_event`).
pub enum AgentEvent {
    /// Someone spoke to the robot: `info` holds the command (text, in_view, from_owner, ...).
    Command {
        info: Value,
        image: Option<bytes::Bytes>,
    },
    /// An arrival or idle time (see `crate::life`), with who is in view; always with an image.
    Nudge {
        info: Value,
        image: Option<bytes::Bytes>,
    },
    /// Time for the agent to start a fresh session.
    EndSession,
    TimedOut {
        focus_minutes_left: Option<u64>,
    },
}

impl Robot {
    /// The next thing for the agent: a command (see `Ears`), a nudge (see `Life`), the end of
    /// its session, or nothing within `timeout`. `include_image`: attach the camera frame to a
    /// command (default: only when it is about seeing).
    /// How what just happened moves its mood (unless it feels something stronger already).
    fn appraise(&self, n: &crate::life::Nudge, company: bool) {
        use crate::life::Nudge;
        use crate::mood::Emotion::*;
        let pets = &self.hub.config.life.pets;
        let (e, i, why) = match n {
            Nudge::Arrived { name } if name.eq_ignore_ascii_case(self.life.owner()) => {
                (Happy, 0.7, format!("{name} is here"))
            }
            Nudge::Arrived { name } => (Happy, 0.55, format!("{name} is here")),
            Nudge::Noticed { what } if !pets.is_empty() && crate::objects::is_animal(what) => {
                let pet = pets[0].split(',').next().unwrap_or_default().trim();
                (Happy, 0.5, format!("saw {pet}"))
            }
            Nudge::Noticed { what } => (Curious, 0.45, format!("spotted a {what}")),
            Nudge::Heard { what } if crate::sounds::ALARMS.contains(&what.as_str()) => {
                (Worried, 0.8, format!("heard: {what}"))
            }
            Nudge::Heard { what } => (Curious, 0.35, format!("heard: {what}")),
            Nudge::Poked => (Surprised, 0.4, "got poked".into()),
            Nudge::Handled { how } => match how.as_str() {
                "pickup" => (Surprised, 0.5, "picked up".into()),
                "shake" => (Grumpy, 0.55, "got shaken".into()),
                "fall" | "upside_down" => (Worried, 0.6, format!("{}", how.replace('_', " "))),
                _ => return,
            },
            Nudge::Battery {
                plugged_in: false, ..
            } => (Sleepy, 0.5, "battery is low".into()),
            Nudge::Battery { .. } => (Content, 0.5, "charging".into()),
            Nudge::Call { from } => (Excited, 0.35, format!("{from} is calling")),
            Nudge::Idle {
                quiet_for_minutes, ..
            } if *quiet_for_minutes >= 60 && !company => {
                (Bored, 0.45, "nothing has happened for a while".into())
            }
            Nudge::Music { .. } => (Playful, 0.45, "music is playing".into()),
            Nudge::Idle { .. } => return,
        };
        self.mood.appraise(e, i, &why);
    }

    pub async fn next_event(
        &self,
        timeout: Duration,
        include_image: Option<bool>,
    ) -> Result<AgentEvent> {
        use tokio::time::Instant;
        let _waiting = self.light.waiting();
        self.ears.start_listening().await?;
        let deadline = Instant::now() + timeout;
        let owner = self.life.owner().to_string();
        let frame = async || self.hub.latest_frame().await.map(|f| f.jpeg.clone());
        loop {
            if let Some(c) = self.ears.take_command() {
                if self.life.focused().is_some() {
                    // Called by name: awake again.
                    self.unfocus();
                }
                self.life.activity();
                let want_image = include_image.unwrap_or_else(|| crate::stt::about_seeing(&c.text));
                let mut info = serde_json::to_value(&c).unwrap_or_default();
                if !owner.is_empty() {
                    // By voice, or by face when the voice is not one it knows.
                    info["from_owner"] = match &c.speaker {
                        Some(s) => s.eq_ignore_ascii_case(&owner),
                        None => c.in_view.iter().any(|n| n.eq_ignore_ascii_case(&owner)),
                    }
                    .into();
                }
                {
                    use crate::mood::Emotion;
                    if matches!(self.mood.now().emotion, Emotion::Bored | Emotion::Sad) {
                        self.mood
                            .appraise(Emotion::Happy, 0.4, "someone talked to me");
                    }
                }
                info["mood"] = self.mood.for_agent();
                let image = if want_image { frame().await } else { None };
                self.light.thinking();
                return Ok(AgentEvent::Command { info, image });
            }
            // Reminders come through focus mode too: someone asked for them.
            if let Some(r) = self
                .reminders
                .take_due(chrono::Local::now().timestamp_millis())
            {
                tracing::info!("reminder due for the agent");
                self.life.activity();
                return Ok(AgentEvent::Nudge {
                    info: json!({
                        "nudge": "reminder",
                        "text": r.text,
                        "set_at": r.set_local(),
                        "due_at": r.due_local(),
                    }),
                    image: None,
                });
            }
            // Its own plans and diary wait for focus mode to end; plans also wait while quiet.
            if self.life.focused().is_none() {
                let now = chrono::Local::now();
                if self.life.activity_level() != crate::life::Activity::Quiet
                    && let Some(p) = self.intentions.take_due(now.timestamp_millis())
                {
                    tracing::info!("plan due for the agent");
                    self.life.activity();
                    return Ok(AgentEvent::Nudge {
                        info: json!({
                            "nudge": "plan",
                            "text": p.text,
                            "planned_at": crate::reminders::local(p.set_ms).format("%a %H:%M").to_string(),
                            "note": "your own plan, now due: do it, put it off (`intend` again) or let it go",
                            "mood": self.mood.for_agent(),
                        }),
                        image: frame().await,
                    });
                }
                if let Ok(at) =
                    chrono::NaiveTime::parse_from_str(&self.hub.config.life.diary_at, "%H:%M")
                    && let Some(day) = self.diary.due(now, at)
                {
                    tracing::info!("diary due for the agent ({day})");
                    self.diary.asked(day);
                    return Ok(AgentEvent::Nudge {
                        info: json!({
                            "nudge": "diary",
                            "day": day.to_string(),
                            "what_happened": self.diary.digest(day),
                            "note": "write your diary for this day with `write_diary`, in your own words: what stood out, how you felt, what you wonder about. Nobody needs to hear it.",
                            "mood": self.mood.for_agent(),
                        }),
                        image: None,
                    });
                }
            }
            if self.life.session_over() {
                return Ok(AgentEvent::EndSession);
            }
            let seen = self.faces.in_view();
            // The camera sits low and often misses faces: people seen without one, or talking
            // nearby, count too.
            let company = !seen.is_empty()
                || self.objects.person_in_view()
                || self.ears.heard_speech_within(COMPANY);
            let tethered = self.tethered().await;
            self.life.set_tethered(tethered);
            if let Some(n) = self.life.poll(company) {
                tracing::info!(
                    "nudge for the agent: {} (company: {company}, faces in view: {})",
                    serde_json::to_string(&n).unwrap_or_default(),
                    seen.len()
                );
                self.appraise(&n, company);
                let mut info = serde_json::to_value(&n).unwrap_or_default();
                info["mood"] = self.mood.for_agent();
                if matches!(n, crate::life::Nudge::Idle { .. }) {
                    let plans: Vec<Value> = self
                        .intentions
                        .some_day(3)
                        .iter()
                        .map(|p| json!({ "id": p.id, "text": p.text }))
                        .collect();
                    if !plans.is_empty() {
                        info["your_plans"] = plans.into();
                    }
                }
                if tethered {
                    info["plugged_in"] = "you are on a cable and cannot drive".into();
                }
                let names: Vec<String> = seen
                    .iter()
                    .map(|s| s.name.clone().unwrap_or_else(|| "someone".into()))
                    .collect();
                info["in_view"] = names.clone().into();
                if matches!(
                    n,
                    crate::life::Nudge::Noticed { .. } | crate::life::Nudge::Heard { .. }
                ) {
                    info["pets"] = self.hub.config.life.pets.clone().into();
                }
                if !owner.is_empty() {
                    info["owner_in_view"] =
                        names.iter().any(|n| n.eq_ignore_ascii_case(&owner)).into();
                }
                // Nudges are about the surroundings: show them. Being handled is not, and a
                // picture slows the reaction down by seconds.
                let image = if matches!(
                    n,
                    crate::life::Nudge::Handled { .. } | crate::life::Nudge::Heard { .. }
                ) {
                    None
                } else {
                    frame().await
                };
                return Ok(AgentEvent::Nudge { info, image });
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(AgentEvent::TimedOut {
                    focus_minutes_left: self.life.focused().map(|d| d.as_secs().div_ceil(60)),
                });
            }
            self.ears
                .wait_command((deadline - now).min(Duration::from_secs(1)))
                .await;
        }
    }
}

/// Being picked up, put down, shaken or dropped, and a low battery, become nudges for the agent.
/// Spawned once by `crate::start`.
///
/// Shakes and falls come from the robot's accelerometer. Its pick-up detection rarely fires, but
/// the bottom IR sensor sees the floor go away when the robot is lifted, and come back when it is
/// put down.
pub async fn on_body(robot: Robot) {
    let mut cursor = robot.hub.last_event_seq().await;
    let bottom = |v: &Value| match v["bottom"].as_str() {
        Some("covered") => Some(true),
        Some("uncovered") => Some(false),
        _ => None,
    };
    // The last reading, if the robot is already connected.
    let mut battery = BatteryWatch::default();
    let mut floor = robot
        .hub
        .status
        .lock()
        .await
        .proximity
        .as_ref()
        .and_then(bottom);
    loop {
        for e in robot
            .hub
            .events_since(cursor, Duration::from_secs(60))
            .await
        {
            cursor = e.seq;
            let how = match e.kind.as_str() {
                "motion" => e.data["kind"].as_str().map(String::from),
                "proximity" => {
                    let now = bottom(&e.data);
                    let was = floor;
                    floor = now.or(floor);
                    match (was, now) {
                        // Unknown before (the plane just started): it normally sits on the floor.
                        (Some(true) | None, Some(false)) => Some("pickup".into()),
                        (Some(false), Some(true)) => Some("putdown".into()),
                        _ => None,
                    }
                }
                "remote" => {
                    remote_button(&robot, &e.data).await;
                    None
                }
                "phone_link" => {
                    let text = if e.data["connected"] == true {
                        "Phone connected."
                    } else {
                        "Phone disconnected."
                    };
                    if let Err(err) = robot.say(text, Some("robot"), false).await {
                        tracing::debug!("phone link: {err:#}");
                    }
                    None
                }
                "phone_call" => {
                    let from = e.data["from"].as_str().unwrap_or("someone");
                    tracing::info!("the phone is ringing: telling the agent");
                    robot.life.call(from);
                    None
                }
                "battery" => {
                    let percent = e.data["pct"].as_f64().unwrap_or(100.0).round() as u32;
                    let plugged_in = e.data["charging"]
                        .as_str()
                        .is_some_and(|c| c != "unplugged");
                    if let Some(warn) = battery.update(percent, plugged_in) {
                        tracing::info!(
                            "battery {percent}% (plugged in: {plugged_in}): telling the agent"
                        );
                        robot.life.battery(warn, plugged_in);
                    }
                    None
                }
                // A new connection reports the sensors afresh.
                "link" => {
                    floor = None;
                    None
                }
                _ => None,
            };
            let Some(how) = how else { continue };
            // Its own driving jolts it, and the bottom sensor misreads while the motors run.
            if robot.is_moving() {
                continue;
            }
            tracing::info!("handled: {how}");
            robot.life.handled(&how);
        }
    }
}

/// A button on the handheld controller, in AI mode.
async fn remote_button(robot: &Robot, data: &Value) {
    let button = data["button"].as_str().unwrap_or_default();
    tracing::info!("remote: {button}");
    match button {
        // The robot leaves AI mode for stock RC mode (it says so) and comes back when that ends.
        "override" => {
            robot
                .manual_override
                .store(true, std::sync::atomic::Ordering::Relaxed);
            robot.cancel_summon().await;
        }
        // Talking, or about to answer: cut it off. Quiet: focus mode.
        "shut_up" => {
            robot.cancel_summon().await;
            if data["talking"] == true || robot.light.answering() {
                robot.hush().await;
            } else {
                let minutes = robot.hub.config.life.default_focus_minutes;
                robot.focus(Duration::from_secs(minutes * 60));
            }
        }
        "poke" => {
            if robot.life.focused().is_some() {
                robot.unfocus();
            }
            robot.ears.conversation_opened();
            robot.life.poked();
        }
        // The remote's Summon button: call the robot to listen, as stock did for voice commands.
        "listen" => {
            if robot.life.focused().is_some() {
                robot.unfocus();
            }
            let robot = robot.clone();
            tokio::spawn(async move {
                if let Err(e) = robot.say("Yes?", Some("robot"), true).await {
                    tracing::debug!("listen: {e:#}");
                }
                // After the prompt: the mic is muted while the robot speaks.
                robot.ears.listen_now();
            });
        }
        "summon" => robot.start_summon().await,
        "other" => tracing::info!("remote sent command {}", data["command"]),
        "scenario" => {
            let name = data["scenario"]
                .as_u64()
                .and_then(|i| bbproto::bb::SCENARIOS.get(i as usize).copied())
                .unwrap_or_default();
            let arg = data["arg"].as_u64().unwrap_or(0);
            // AI mode's own menu entries that need no agent.
            match name {
                // The view itself talks to the plane over Wi-Fi (see `crate::peek`).
                "ai_peek" => return,
                "ai_focus" => {
                    let minutes = robot.hub.config.life.default_focus_minutes;
                    robot.focus(Duration::from_secs(minutes * 60));
                    return;
                }
                "ai_activity" => {
                    use crate::life::Activity;
                    let level = [
                        Activity::Quiet,
                        Activity::Calm,
                        Activity::Normal,
                        Activity::Lively,
                    ]
                    .get((arg & 0x03) as usize)
                    .copied()
                    .unwrap_or_default();
                    robot.set_activity(level);
                    // The joystick's quick change (bit 7) shows on the screen; it says nothing.
                    if arg & 0x80 != 0 {
                        return;
                    }
                    let text = format!("Activity: {}.", level.name());
                    if let Err(e) = robot.say(&text, Some("robot"), false).await {
                        tracing::debug!("activity: {e:#}");
                    }
                    return;
                }
                _ => {}
            }
            if let Some(text) = menu_request(name, arg) {
                robot.ears.command(&text);
                robot.light.thinking();
            }
        }
        _ => {}
    }
}

/// What a controller menu entry asks the agent, for the entries the robot leaves to it.
fn menu_request(scenario: &str, arg: u64) -> Option<String> {
    const DICE: [&str; 8] = ["", "4", "6", "8", "10", "12", "20", "100"];
    Some(match scenario {
        "eight_ball" => {
            "Be a magic eight ball: ask me for my yes-or-no question, then answer it.".into()
        }
        "dice_roll" => match DICE.get(arg as usize).copied().unwrap_or("") {
            "" => "Roll a die for me.".into(),
            n => format!("Roll a d{n} for me."),
        },
        "voice_control" => match arg {
            0 => "Drive forward a little.",
            1 => "Back up a little.",
            2 => "Turn left.",
            3 => "Turn right.",
            _ => "Turn around.",
        }
        .into(),
        "dance" => "Dance for me!".into(),
        "face_detect" if arg == 1 => "Remember my face.".into(),
        "face_detect" => "Who's this in front of you?".into(),
        "face_detect_forget" => "Forget my face.".into(),
        "whats_this" => "What's this in front of you?".into(),
        "read_notifs" | "phone_list_notifs" => "Do I have any notifications?".into(),
        "ir_train" => "Learn a button from my IR remote.".into(),
        "ir_forget" => "Forget a button from my IR remote.".into(),
        "ai_see" => "What do you see right now?".into(),
        "ai_news" => "Tell me about something new in one of the things I'm into.".into(),
        "ai_weather" => "What's the weather like today?".into(),
        "ai_explore" => "Go explore for a bit, then tell me what you found.".into(),
        "ai_story" => "Tell me a very short story.".into(),
        _ => return None,
    })
}

/// A reminder nobody passed on for a minute (no agent running, or it is paused) is said by the
/// robot itself. Spawned once by `crate::start`.
pub async fn overdue_reminders(robot: Robot) {
    const GRACE_MS: i64 = 60_000;
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let now = chrono::Local::now().timestamp_millis();
        if !robot.reminders.overdue(now, GRACE_MS)
            || !robot.hub.is_connected().await
            || robot.mode().await.as_deref() != Some("remote")
        {
            continue;
        }
        while let Some(r) = robot.reminders.take_due(now - GRACE_MS) {
            let text = format!("Reminder: {}", r.text);
            if let Err(e) = robot.say(&text, Some("robot"), true).await {
                tracing::warn!("saying a reminder: {e:#}");
            }
        }
    }
}

/// When to tell the agent about the battery: at 20% and 10% while unplugged, and when it is
/// plugged in after such a warning.
#[derive(Default)]
struct BatteryWatch {
    /// The lowest warning given since it was last charged.
    warned_at: Option<u32>,
}

impl BatteryWatch {
    const LEVELS: [u32; 2] = [20, 10];

    /// The percentage to report, if the agent should hear about it.
    fn update(&mut self, percent: u32, plugged_in: bool) -> Option<u32> {
        if plugged_in {
            return self.warned_at.take().map(|_| percent);
        }
        if percent > Self::LEVELS[0] + 5 {
            self.warned_at = None; // charged up again without us noticing the plug
            return None;
        }
        let due = Self::LEVELS
            .iter()
            .copied()
            .filter(|l| percent <= *l && self.warned_at.is_none_or(|w| *l < w))
            .min()?;
        self.warned_at = Some(due);
        Some(percent)
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
                    // Back from Manual Override (stock RC mode), if that is where it was.
                    robot
                        .manual_override
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    let robot = robot.clone();
                    let woken_by_plane = robot
                        .plane_woke
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    tokio::spawn(async move {
                        if let Err(e) = robot.apply_session_settings().await {
                            tracing::warn!("starting the robot's streams: {e:#}");
                        }
                        if robot.hub.config.announce_connect && !woken_by_plane {
                            // Like stock's own announcement, mention a linked phone.
                            let phone =
                                tokio::time::timeout(Duration::from_secs(2), robot.phone("status"))
                                    .await
                                    .ok()
                                    .and_then(Result::ok)
                                    .is_some_and(|s| s["connected"] == true);
                            let text = if phone {
                                "Connected. Your phone is linked."
                            } else {
                                "Connected."
                            };
                            if let Err(e) = robot.say(text, Some("robot"), false).await {
                                tracing::debug!("connect announcement: {e:#}");
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    }
}

/// Speech played while it is still arriving (a realtime model's audio, streamed TTS).
/// Created by `Robot::speaker_stream`; feed it with `push`, end it with `finish`.
pub struct SpeakerStream {
    tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<i16>>>,
    task: tokio::task::JoinHandle<Result<()>>,
    resampler: Resampler,
}

impl SpeakerStream {
    /// Queue mono PCM at the stream's input rate.
    pub fn push(&mut self, pcm: &[i16]) {
        let out = self.resampler.process(pcm);
        if !out.is_empty()
            && let Some(tx) = &self.tx
        {
            let _ = tx.send(out);
        }
    }

    /// No more audio: wait until the robot has played everything.
    pub async fn finish(mut self) -> Result<()> {
        self.tx = None;
        (&mut self.task).await?
    }
}

/// Linear-interpolating resampler to the robot's 16 kHz, keeping its position across chunks.
struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    fn new(from_rate: u32) -> Resampler {
        Resampler {
            step: from_rate as f64 / RATE as f64,
            // Position 1.0 is the first sample of the first chunk (0.0 the one before it).
            pos: 1.0,
            last: 0.0,
        }
    }

    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        if (self.step - 1.0).abs() < 1e-9 {
            return input.to_vec();
        }
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 1);
        // `pos` counts from the sample before `input` (the previous chunk's last one).
        let at = |i: isize| -> f32 {
            if i < 0 {
                self.last
            } else {
                input[i as usize] as f32
            }
        };
        while self.pos < input.len() as f64 {
            let i = self.pos.floor() as isize - 1;
            let f = (self.pos - self.pos.floor()) as f32;
            let v = at(i) * (1.0 - f) + at(i + 1) * f;
            out.push(v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        if let Some(&l) = input.last() {
            self.last = l as f32;
        }
        out
    }
}

impl Robot {
    /// Start streaming speech at `rate` Hz to the robot speaker (stopping anything playing).
    pub async fn speaker_stream(&self, rate: u32) -> Result<SpeakerStream> {
        self.ensure_remote().await?;
        let token = CancellationToken::new();
        if let Some(old) = self.speaker.lock().await.replace(token.clone()) {
            old.cancel();
            self.hub.send(ToRobot::SpkAbort).await.ok();
        }
        let id = self.hub.next_id();
        self.hub.request(ToRobot::SpkBegin { id }).await?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = self.hub.clone();
        Ok(SpeakerStream {
            tx: Some(tx),
            task: tokio::spawn(pace_stream(hub, rx, token)),
            resampler: Resampler::new(rate),
        })
    }
}

/// Audio collected before a stream starts playing: streamed speech arrives in bursts, and the
/// speaker running dry crackles.
const STREAM_PREBUFFER: usize = (RATE as usize * 3) / 10;

/// Send queued 16 kHz audio to the robot about SPK_LEAD ahead of playback, as it arrives.
async fn pace_stream(
    hub: Arc<Hub>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>,
    token: CancellationToken,
) -> Result<()> {
    let mut buf: std::collections::VecDeque<i16> = Default::default();
    let mut start: Option<tokio::time::Instant> = None;
    let mut sent = 0usize;
    let mut open = true;
    loop {
        // Take in whatever has arrived.
        while let Ok(more) = rx.try_recv() {
            buf.extend(more);
        }
        let waiting_to_start = start.is_none() && open && buf.len() < STREAM_PREBUFFER;
        if buf.is_empty() || waiting_to_start {
            if !open {
                break;
            }
            tokio::select! {
                _ = token.cancelled() => return Ok(()),
                more = rx.recv() => match more {
                    Some(more) => buf.extend(more),
                    None => open = false,
                },
            }
            continue;
        }
        let now = tokio::time::Instant::now();
        let played = Duration::from_secs_f64(sent as f64 / RATE as f64);
        // First audio, or the stream ran dry (the robot has played everything): restart the clock.
        let t0 = match start {
            Some(t) if t + played > now => t,
            _ => {
                let t = now - played;
                start = Some(t);
                t
            }
        };
        let due = t0 + played.saturating_sub(SPK_LEAD);
        if due > now {
            tokio::select! {
                _ = token.cancelled() => return Ok(()),
                _ = tokio::time::sleep_until(due) => {}
            }
        }
        let n = buf.len().min(SPK_CHUNK);
        let chunk: Vec<i16> = buf.drain(..n).collect();
        hub.send_frame(Frame::new(
            FrameType::SpkPcm,
            bbproto::frame::pcm_to_le(&chunk),
        ))
        .await?;
        sent += n;
    }
    hub.send(ToRobot::SpkEnd).await?;
    if let Some(t0) = start {
        let total = Duration::from_secs_f64(sent as f64 / RATE as f64);
        tokio::select! {
            _ = token.cancelled() => {}
            _ = tokio::time::sleep_until(t0 + total + Duration::from_millis(100)) => {}
        }
    }
    Ok(())
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

#[cfg(test)]
mod resampler_tests {
    use super::Resampler;

    #[test]
    fn resamples_24k_to_16k_across_chunks() {
        let tone: Vec<i16> = (0..24_000)
            .map(|i| {
                (8000.0 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 24_000.0).sin()) as i16
            })
            .collect();
        let mut r = Resampler::new(24_000);
        let mut out = Vec::new();
        for c in tone.chunks(777) {
            out.extend(r.process(c));
        }
        assert!((15_990..=16_010).contains(&out.len()), "{}", out.len());
        // Still a clean 440 Hz tone: compare with the ideal at 16 kHz.
        let err: f32 = out[100..15_900]
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let t = (i + 100) as f32 / 16_000.0;
                (v as f32 - 8000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()).abs()
            })
            .sum::<f32>()
            / 15_800.0;
        assert!(err < 200.0, "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::{BatteryWatch, menu_request};

    #[test]
    fn menu_entries_the_robot_cannot_run_become_requests() {
        assert_eq!(menu_request("dice_roll", 6).unwrap(), "Roll a d20 for me.");
        assert_eq!(menu_request("dice_roll", 0).unwrap(), "Roll a die for me.");
        assert_eq!(menu_request("voice_control", 2).unwrap(), "Turn left.");
        assert_eq!(menu_request("face_detect", 1).unwrap(), "Remember my face.");
        assert!(
            menu_request("phone_list_notifs", 0)
                .unwrap()
                .contains("notifications")
        );
        assert!(
            menu_request("joke", 0).is_none(),
            "the robot tells jokes itself"
        );
    }

    #[test]
    fn battery_warns_twice_then_thanks_once() {
        let mut b = BatteryWatch::default();
        assert_eq!(b.update(60, false), None);
        assert_eq!(b.update(20, false), Some(20));
        assert_eq!(b.update(19, false), None, "warned at this level already");
        assert_eq!(b.update(9, false), Some(9));
        assert_eq!(b.update(8, false), None);
        assert_eq!(b.update(8, true), Some(8), "plugged in after a warning");
        assert_eq!(b.update(30, true), None);
        // Unplugged at a high level: nothing to say. A plug without a warning: nothing either.
        assert_eq!(b.update(90, false), None);
        assert_eq!(b.update(90, true), None);
        // Jumping straight to 10% gives one warning, not two.
        assert_eq!(b.update(9, false), Some(9));
        assert_eq!(b.update(9, false), None);
    }
}
