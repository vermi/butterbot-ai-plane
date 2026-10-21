//! Sound recognition in AI mode, on the laptop.
//!
//! The robot's microphone stream goes through YAMNet twice a second (the last 0.96 s each time).
//! When one of the `notice` sounds is heard (a bark, the doorbell, a smoke alarm...), the agent
//! gets a nudge. Quiet stretches are skipped, and louder ones are brought to a common level first:
//! the robot's microphone is quiet. Short sounds (a bark, a knock) barely move the average level of
//! a second of audio, so both go by the loudest moment.

pub mod engine;
pub mod tempo;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{OnceCell, broadcast::error::RecvError};
use tokio::time::Instant;

use crate::config::SoundsConfig;
use crate::control::Robot;
use engine::{Engine, WINDOW};

/// Classify every this many new samples (half a window).
const HOP: usize = WINDOW / 2;
/// A window whose loudest moment stays below this (about -42 dBFS) is room noise.
const NOISE_FLOOR: f32 = 0.008;
/// Louder windows are scaled so their loudest moment reaches this before classifying.
const TARGET_PEAK: f32 = 0.5;
/// Sounds that matter even in focus mode.
pub const ALARMS: [&str; 3] = ["Smoke detector, smoke alarm", "Fire alarm", "Alarm"];
/// An alarm keeps going: it must be heard in this many windows in a row (about 1.5 s) at
/// `ALARM_SCORE` or more. A tap on the counter, levelled up, briefly scores like one.
const ALARM_WINDOWS: u32 = 3;
const ALARM_SCORE: f32 = 0.5;
/// Quiet windows are brought up at most this much.
const MAX_GAIN: f32 = 8.0;
/// Music: YAMNet's "Music" class at least this strong...
const MUSIC_SCORE: f32 = 0.3;
/// ...in this many windows (half-second steps, losing one for each window without) means music is
/// playing; none for `MUSIC_GONE` means it stopped.
const MUSIC_WINDOWS: u32 = 8;
const MUSIC_GONE: Duration = Duration::from_secs(30);
/// After a failed model load (usually the download), wait this long before trying again.
const LOAD_RETRY: Duration = Duration::from_secs(60);

pub struct Sounds {
    cfg: SoundsConfig,
    models_dir: PathBuf,
    engine: OnceCell<Arc<Engine>>,
    load_failure: Mutex<Option<(Instant, String)>>,
    last_heard: Mutex<HashMap<String, Instant>>,
    /// Windows in a row each alarm has been heard in.
    alarm_streak: Mutex<HashMap<String, u32>>,
    /// The most recent sounds heard (any class), for `status`.
    recent: Mutex<VecDeque<(String, f32, Instant)>>,
    /// Music: windows with it (see `MUSIC_WINDOWS`), when it was last heard, whether it counts
    /// as playing.
    music: Mutex<(u32, Option<Instant>, bool)>,
    /// The beat of what it hears.
    tempo: Mutex<tempo::Tempo>,
    /// Music just started (with its beat): for `run` to pass on.
    music_started: Mutex<Option<Option<f64>>>,
}

impl Sounds {
    pub fn new(hub: &crate::hub::Hub) -> Sounds {
        Sounds {
            cfg: hub.config.sounds.clone(),
            models_dir: hub.config.data_dir().join("models"),
            engine: OnceCell::new(),
            load_failure: Mutex::new(None),
            last_heard: Mutex::new(HashMap::new()),
            alarm_streak: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            music: Mutex::new((0, None, false)),
            tempo: Mutex::new(tempo::Tempo::default()),
            music_started: Mutex::new(None),
        }
    }

    /// Whether music is playing nearby, and its beat if there is a clear one.
    pub fn music(&self) -> Option<Option<f64>> {
        let m = self.music.lock().unwrap();
        m.2.then(|| self.tempo.lock().unwrap().bpm())
    }

    /// One window's music score; returns true when music just started.
    fn track_music(&self, score: f32) -> bool {
        let mut m = self.music.lock().unwrap();
        let now = Instant::now();
        if score >= MUSIC_SCORE {
            m.0 = (m.0 + 1).min(MUSIC_WINDOWS * 2);
            m.1 = Some(now);
        } else {
            m.0 = m.0.saturating_sub(1);
        }
        if !m.2 && m.0 >= MUSIC_WINDOWS {
            m.2 = true;
            return true;
        }
        if m.2 && m.1.is_none_or(|t| now - t >= MUSIC_GONE) {
            *m = (0, None, false);
            tracing::info!("music stopped");
        }
        false
    }

    pub fn status_json(&self) -> Value {
        let recent: Vec<Value> = self
            .recent
            .lock()
            .unwrap()
            .iter()
            .map(|(c, s, at)| json!({ "class": c, "score": s, "secs_ago": at.elapsed().as_secs() }))
            .collect();
        json!({ "enabled": self.cfg.enabled, "notice": self.cfg.notice, "recent": recent })
    }

    async fn engine(&self) -> Result<Arc<Engine>> {
        if let Some(e) = self.engine.get() {
            return Ok(e.clone());
        }
        if let Some((at, why)) = self.load_failure.lock().unwrap().as_ref()
            && at.elapsed() < LOAD_RETRY
        {
            bail!(
                "{why} (retrying in {}s)",
                (LOAD_RETRY - at.elapsed()).as_secs()
            );
        }
        let dir = self.models_dir.clone();
        let r = self
            .engine
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || Engine::load(&dir).map(Arc::new))
                    .await
                    .map_err(|e| anyhow!("model loader panicked: {e}"))?
                    .context("loading the sound recognition model")
            })
            .await
            .cloned();
        if let Err(e) = &r {
            *self.load_failure.lock().unwrap() = Some((Instant::now(), format!("{e:#}")));
        }
        r
    }

    /// Classify one window; returns the `notice` sounds heard, not heard within the cooldown.
    async fn listen(&self, window: Vec<f32>) -> Result<Vec<(String, f32)>> {
        let Some(scaled) = level(window) else {
            self.track_music(0.0);
            return Ok(Vec::new());
        };
        let engine = self.engine().await?;
        let ranked: Vec<(String, f32)> = tokio::task::spawn_blocking(move || {
            engine
                .classify(&scaled)
                .map(|r| r.into_iter().map(|(c, s)| (c.to_string(), s)).collect())
        })
        .await
        .map_err(|e| anyhow!("sound recognition panicked: {e}"))??;
        let music = ranked
            .iter()
            .find(|(c, _)| c == "Music")
            .map_or(0.0, |(_, s)| *s);
        if self.track_music(music) {
            let bpm = self.tempo.lock().unwrap().bpm();
            tracing::info!("music playing (beat: {bpm:?})");
            self.music_started.lock().unwrap().replace(bpm);
        }
        let now = Instant::now();
        if let Some((top, score)) = ranked.first()
            && *score >= self.cfg.threshold
            && top != "Silence"
        {
            let mut recent = self.recent.lock().unwrap();
            recent.push_back((top.clone(), *score, now));
            while recent.len() > 10 {
                recent.pop_front();
            }
        }
        // Alarms count only when they go on for a while.
        let mut streaks = self.alarm_streak.lock().unwrap();
        for alarm in ALARMS {
            let loud = ranked
                .iter()
                .any(|(c, s)| c == alarm && *s >= ALARM_SCORE.max(self.cfg.threshold));
            let n = streaks.entry(alarm.to_string()).or_insert(0);
            *n = if loud { *n + 1 } else { 0 };
        }
        let cooldown = Duration::from_secs(self.cfg.cooldown_secs);
        let mut last = self.last_heard.lock().unwrap();
        Ok(ranked
            .into_iter()
            .filter(|(c, s)| *s >= self.cfg.threshold && self.cfg.notice.contains(c))
            .filter(|(c, _)| {
                !ALARMS.contains(&c.as_str()) || streaks.get(c).is_some_and(|n| *n >= ALARM_WINDOWS)
            })
            .filter(|(c, _)| {
                let fresh = last.get(c).is_none_or(|t| now - *t >= cooldown);
                if fresh {
                    last.insert(c.clone(), now);
                }
                fresh
            })
            .collect())
    }
}

/// `window` brought to a common loudness, or None if it is only room noise.
fn level(mut window: Vec<f32>) -> Option<Vec<f32>> {
    let peak = window.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak < NOISE_FLOOR {
        return None;
    }
    let gain = (TARGET_PEAK / peak).clamp(1.0, MAX_GAIN);
    for s in &mut window {
        *s = (*s * gain).clamp(-1.0, 1.0);
    }
    Some(window)
}

/// Listen to the microphone while the robot streams it. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let sounds = robot.sounds.clone();
    if !sounds.cfg.enabled {
        return;
    }
    let hub = robot.hub.clone();
    let mut rx = hub.mic.subscribe();
    let mut buf: VecDeque<f32> = VecDeque::with_capacity(WINDOW);
    let mut fresh = 0usize;
    let mut last_error = String::new();
    loop {
        let chunk = match rx.recv().await {
            Ok(c) => c,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        // Its own voice is not news (the robot also mutes the mic while it speaks).
        if hub.status.lock().await.speaker_playing {
            buf.clear();
            fresh = 0;
            sounds.tempo.lock().unwrap().reset();
            continue;
        }
        sounds.tempo.lock().unwrap().push(&chunk);
        for &s in chunk.iter() {
            if buf.len() == WINDOW {
                buf.pop_front();
            }
            buf.push_back(s as f32 / 32768.0);
        }
        fresh += chunk.len();
        if buf.len() < WINDOW || fresh < HOP {
            continue;
        }
        fresh = 0;
        let heard = match sounds.listen(buf.iter().copied().collect()).await {
            Ok(h) => {
                last_error.clear();
                h
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg != last_error {
                    tracing::warn!("sound recognition: {msg}");
                    last_error = msg;
                }
                continue;
            }
        };
        let started = sounds.music_started.lock().unwrap().take();
        if let Some(bpm) = started {
            hub.push_event("sound", json!({ "heard": "music", "bpm": bpm }))
                .await;
            robot
                .life
                .music("music playing nearby", bpm.map(|b| b.round() as u32));
        }
        for (what, score) in heard {
            tracing::info!("heard: {what} ({score:.2})");
            hub.push_event("sound", json!({ "heard": what, "score": score }))
                .await;
            robot.life.heard(&what, ALARMS.contains(&what.as_str()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_is_skipped_and_loud_is_levelled() {
        assert!(level(vec![0.001; WINDOW]).is_none());
        // A short knock in a quiet second still counts, and is brought up.
        let mut knock = vec![0.001; WINDOW];
        knock[100] = 0.1;
        let up = level(knock).unwrap();
        assert!((up[100] - TARGET_PEAK).abs() < 1e-4, "{}", up[100]);
        // Never more than MAX_GAIN.
        let mut faint = vec![0.0; WINDOW];
        faint[0] = 0.01;
        assert!((level(faint).unwrap()[0] - 0.01 * MAX_GAIN).abs() < 1e-5);
        let loud = level(vec![0.5; WINDOW]).unwrap();
        assert_eq!(loud[0], 0.5, "never turned down");
    }
}
