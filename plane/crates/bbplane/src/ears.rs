//! Listening for commands in AI mode.
//!
//! Once an agent has asked to `hear` something, the robot's microphone stream is cut into
//! utterances (by loudness against the measured noise floor) and each one is transcribed on this
//! machine. Only utterances that address the robot by its wake word ("Butterbot, ...") become
//! commands; everything else is dropped as soon as it is transcribed, never stored or passed on.
//! Commands wait in a short queue, so one spoken while the agent is busy is not lost.
//!
//! When the robot finishes speaking in reply to a command, it stays addressed for
//! `follow_up_secs`: anything said in that window is a command too, without the wake word.

use std::collections::VecDeque;
#[cfg(feature = "stt")]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::json;
#[cfg(feature = "stt")]
use tokio::sync::OnceCell;
use tokio::sync::{Notify, broadcast::error::RecvError};
use tokio::time::Instant;

use crate::config::SttConfig;
use crate::control::Robot;
use crate::hub::{Hub, now_ms};

const RATE: f32 = bbproto::SAMPLE_RATE as f32;
/// Audio kept from before speech was detected, so the first syllable is not cut off.
const PRE_ROLL: Duration = Duration::from_millis(400);
/// Speech ends after this much quiet.
const END_SILENCE: Duration = Duration::from_millis(500);
/// Quiet means the smoothed level is below this fraction of the utterance's peak (so a busy room
/// does not keep it going) or near the noise floor.
const PEAK_KEEP: f32 = 0.25;
/// Chunks averaged for the smoothed level (about 160 ms).
const SMOOTH_CHUNKS: usize = 5;
/// After the robot's name alone ("Butterbot." then a pause), the next utterance within this
/// long is the command.
const NAME_ONLY_WINDOW: Duration = Duration::from_secs(8);
/// Utterances with less loud time than this are clicks and bumps, not speech.
const MIN_VOICED: Duration = Duration::from_millis(250);
const MAX_UTTERANCE: Duration = Duration::from_secs(15);
/// Loudness (RMS) relative to the noise floor that starts and sustains an utterance.
const START_RATIO: f32 = 1.8;
const KEEP_RATIO: f32 = 1.3;
/// Chunks in a row above START_RATIO needed to start (about 100 ms).
const START_CHUNKS: u32 = 3;
#[cfg(feature = "stt")]
const LOAD_RETRY: Duration = Duration::from_secs(60);
const QUEUE_LEN: usize = 8;
/// The robot finishing speech opens the follow-up window only if a command came this recently
/// (otherwise it was something else talking, such as a greeting).
const CONVERSATION: Duration = Duration::from_secs(300);

/// Something said to the robot.
#[derive(Debug, Clone, Serialize)]
pub struct Command {
    /// What was asked, without the wake word.
    pub text: String,
    /// The whole transcript.
    pub said: String,
    /// Said in the follow-up window, without the wake word.
    pub follow_up: bool,
    /// Unix time in milliseconds when the speaker stopped.
    pub at_ms: u64,
    /// Enrolled people in front of the camera at the time (likely the speaker).
    pub in_view: Vec<String>,
    #[serde(skip)]
    heard_at: Option<Instant>,
}

pub struct Ears {
    cfg: SttConfig,
    #[cfg(feature = "stt")]
    models_dir: PathBuf,
    #[cfg(feature = "stt")]
    engine: OnceCell<Arc<crate::stt::Stt>>,
    #[cfg(feature = "stt")]
    load_failure: Mutex<Option<(std::time::Instant, String)>>,
    /// Set once something asked for commands; until then nothing is transcribed.
    active: AtomicBool,
    commands: Mutex<VecDeque<Command>>,
    arrived: Notify,
    /// When the last command was heard, and until when no wake word is needed.
    last_command: Mutex<Option<Instant>>,
    open_until: Mutex<Option<Instant>>,
}

impl Ears {
    pub fn new(hub: Arc<Hub>) -> Ears {
        Ears {
            cfg: hub.config.stt.clone(),
            #[cfg(feature = "stt")]
            models_dir: hub.config.data_dir().join("models"),
            #[cfg(feature = "stt")]
            engine: OnceCell::new(),
            #[cfg(feature = "stt")]
            load_failure: Mutex::new(None),
            active: AtomicBool::new(false),
            commands: Mutex::new(VecDeque::new()),
            arrived: Notify::new(),
            last_command: Mutex::new(None),
            open_until: Mutex::new(None),
        }
    }

    pub fn wake_word(&self) -> &str {
        &self.cfg.wake_word
    }

    /// Text spoken in `pcm` (16 kHz mono).
    pub async fn transcribe(&self, pcm: Vec<i16>) -> Result<String> {
        if !self.cfg.enabled {
            bail!("speech-to-text is off ([stt] enabled = false)");
        }
        #[cfg(feature = "stt")]
        {
            let stt = self.engine().await?;
            tokio::task::spawn_blocking(move || stt.transcribe(&pcm))
                .await
                .map_err(|e| anyhow::anyhow!("transcription panicked: {e}"))?
        }
        #[cfg(not(feature = "stt"))]
        {
            let _ = pcm;
            bail!("bbplane was built without speech-to-text (the `stt` feature)")
        }
    }

    #[cfg(feature = "stt")]
    async fn engine(&self) -> Result<Arc<crate::stt::Stt>> {
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
                tokio::task::spawn_blocking(move || crate::stt::Stt::load(&dir).map(Arc::new))
                    .await
                    .map_err(|e| anyhow::anyhow!("model loader panicked: {e}"))?
            })
            .await
            .cloned();
        if let Err(e) = &r {
            *self.load_failure.lock().unwrap() =
                Some((std::time::Instant::now(), format!("{e:#}")));
        }
        r
    }

    /// The next command, waiting up to `timeout` for one. Starts listening on first use (and
    /// loads the speech model, downloading it once).
    pub async fn next_command(&self, timeout: Duration) -> Result<Option<Command>> {
        if !self.cfg.enabled {
            bail!("speech-to-text is off ([stt] enabled = false)");
        }
        if !self.active.swap(true, Ordering::Relaxed) {
            tracing::info!("listening for \"{}, ...\"", self.cfg.wake_word);
        }
        #[cfg(feature = "stt")]
        self.engine().await?;
        let max_age = Duration::from_secs(self.cfg.command_max_age_secs);
        let deadline = Instant::now() + timeout;
        loop {
            let arrived = self.arrived.notified();
            {
                let mut q = self.commands.lock().unwrap();
                while let Some(c) = q.pop_front() {
                    if c.heard_at.is_some_and(|t| t.elapsed() <= max_age) {
                        return Ok(Some(c));
                    }
                }
            }
            if tokio::time::timeout_at(deadline, arrived).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// Whether speech now counts as a command without the wake word.
    fn is_open(&self) -> bool {
        self.open_until
            .lock()
            .unwrap()
            .is_some_and(|t| Instant::now() < t)
    }

    /// Treat whatever is said in the next `d` as addressed to the robot.
    fn open_for(&self, d: Duration) {
        let until = Instant::now() + d;
        let mut o = self.open_until.lock().unwrap();
        if o.is_none_or(|t| t < until) {
            *o = Some(until);
        }
    }

    /// The robot stopped speaking: if that was (likely) an answer, keep listening for a reply.
    fn speech_ended(&self) {
        let recent = self
            .last_command
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < CONVERSATION);
        if recent {
            *self.open_until.lock().unwrap() =
                Some(Instant::now() + Duration::from_secs(self.cfg.follow_up_secs));
        }
    }

    fn push(&self, c: Command) {
        *self.last_command.lock().unwrap() = Some(Instant::now());
        // Also open the window now, in case the agent answers without speaking.
        *self.open_until.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(self.cfg.follow_up_secs));
        let mut q = self.commands.lock().unwrap();
        q.push_back(c);
        while q.len() > QUEUE_LEN {
            q.pop_front();
        }
        drop(q);
        self.arrived.notify_waiters();
    }
}

/// Cuts the microphone stream into utterances by loudness.
struct Segmenter {
    floor: f32,
    loud_run: u32,
    in_speech: bool,
    quiet: f32,
    /// Seconds of the current utterance that were above the noise floor.
    voiced: f32,
    /// Recent chunk levels, for a smoothed level.
    recent: VecDeque<f32>,
    /// Loudest smoothed level of the current utterance.
    peak: f32,
    pre: VecDeque<Vec<i16>>,
    pre_len: usize,
    utterance: Vec<i16>,
}

impl Segmenter {
    fn new(floor: f32) -> Segmenter {
        Segmenter {
            floor: floor.max(10.0),
            loud_run: 0,
            in_speech: false,
            quiet: 0.0,
            voiced: 0.0,
            recent: VecDeque::new(),
            peak: 0.0,
            pre: VecDeque::new(),
            pre_len: 0,
            utterance: Vec::new(),
        }
    }

    /// Feed one chunk; returns a finished utterance.
    fn push(&mut self, chunk: &[i16]) -> Option<Vec<i16>> {
        if chunk.is_empty() {
            return None;
        }
        let secs = chunk.len() as f32 / RATE;
        let rms =
            (chunk.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / chunk.len() as f32).sqrt();
        // All zeros: the robot muted its microphone because it is speaking.
        let gated = rms < 1.0;
        self.recent.push_back(rms);
        while self.recent.len() > SMOOTH_CHUNKS {
            self.recent.pop_front();
        }
        if self.in_speech {
            self.utterance.extend_from_slice(chunk);
            let level = self.recent.iter().sum::<f32>() / self.recent.len() as f32;
            self.peak = self.peak.max(level);
            let keep = (self.floor * KEEP_RATIO).max(self.peak * PEAK_KEEP);
            if !gated && rms > keep {
                self.voiced += secs;
            }
            if !gated && level > keep {
                self.quiet = 0.0;
            } else {
                self.quiet += secs;
            }
            let len = self.utterance.len() as f32 / RATE;
            if gated
                || self.quiet >= END_SILENCE.as_secs_f32()
                || len >= MAX_UTTERANCE.as_secs_f32()
            {
                self.in_speech = false;
                self.quiet = 0.0;
                let u = std::mem::take(&mut self.utterance);
                return (self.voiced >= MIN_VOICED.as_secs_f32()).then_some(u);
            }
            return None;
        }
        if gated {
            self.loud_run = 0;
            self.pre.clear();
            self.pre_len = 0;
            return None;
        }
        self.pre.push_back(chunk.to_vec());
        self.pre_len += chunk.len();
        while self.pre_len > (PRE_ROLL.as_secs_f32() * RATE) as usize {
            let old = self.pre.pop_front().unwrap();
            self.pre_len -= old.len();
        }
        if rms > self.floor * START_RATIO && rms > self.floor + 20.0 {
            self.loud_run += 1;
            if self.loud_run >= START_CHUNKS {
                self.in_speech = true;
                self.voiced = START_CHUNKS as f32 * secs;
                self.peak = self.recent.iter().sum::<f32>() / self.recent.len() as f32;
                self.loud_run = 0;
                self.utterance = self.pre.drain(..).flatten().collect();
                self.pre_len = 0;
            }
        } else {
            self.loud_run = 0;
            // Follow the room's noise level slowly, faster downwards.
            let a = if rms < self.floor { 0.05 } else { 0.01 };
            self.floor = (self.floor * (1.0 - a) + rms * a).max(10.0);
        }
        None
    }
}

/// Watch the microphone while the robot streams it. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let ears = robot.ears.clone();
    let hub = robot.hub.clone();
    let mut rx = hub.mic.subscribe();
    let mut seg: Option<Segmenter> = None;
    let mut speaking = false;
    loop {
        let chunk = match rx.recv().await {
            Ok(c) => c,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let now_speaking = hub.status.lock().await.speaker_playing;
        if speaking && !now_speaking {
            ears.speech_ended();
        }
        speaking = now_speaking;
        if !ears.cfg.enabled || !ears.active.load(Ordering::Relaxed) {
            seg = None;
            continue;
        }
        let s = seg.get_or_insert_with(|| {
            // Start from the recent noise level.
            Segmenter::new(initial_floor(&chunk))
        });
        let Some(utterance) = s.push(&chunk) else {
            continue;
        };
        let ears = ears.clone();
        let robot = robot.clone();
        let secs = utterance.len() as f32 / RATE;
        tokio::spawn(async move {
            let started = Instant::now();
            let said = match ears.transcribe(utterance).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("transcription: {e:#}");
                    return;
                }
            };
            // The words themselves are only logged at debug level (opt-in, for diagnosing the
            // wake word), never by default.
            tracing::debug!(
                "heard {secs:.1} s in {} ms: {said:?}",
                started.elapsed().as_millis()
            );
            if said.is_empty() {
                tracing::info!("{secs:.1} s of sound, no speech");
                return;
            }
            let (text, follow_up) = match crate::stt::addressed(&said, &ears.cfg.wake_word) {
                // Just the name, then (probably) a pause: the command comes next.
                Some("") => {
                    tracing::info!("called by name; listening for the command");
                    ears.open_for(NAME_ONLY_WINDOW);
                    return;
                }
                Some(t) => (t.to_string(), false),
                None if ears.is_open() => (said.clone(), true),
                None => {
                    // Not for the robot: forget it.
                    tracing::info!("{secs:.1} s of speech not addressed to the robot, ignored");
                    return;
                }
            };
            let in_view: Vec<String> = robot
                .faces
                .in_view()
                .into_iter()
                .filter_map(|s| s.name)
                .collect();
            let c = Command {
                text,
                said: said.clone(),
                follow_up,
                at_ms: now_ms(),
                in_view: in_view.clone(),
                heard_at: Some(Instant::now()),
            };
            robot
                .hub
                .push_event(
                    "heard",
                    json!({ "text": c.text, "follow_up": follow_up, "in_view": in_view }),
                )
                .await;
            ears.push(c);
        });
    }
}

fn initial_floor(chunk: &[i16]) -> f32 {
    let rms =
        (chunk.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / chunk.len().max(1) as f32).sqrt();
    rms.clamp(10.0, 400.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(level: f32, n: usize, t0: usize) -> Vec<i16> {
        (0..n)
            .map(|i| (level * 1.414 * ((t0 + i) as f32 * 0.3).sin()) as i16)
            .collect()
    }

    #[test]
    fn segments_speech_between_noise() {
        let mut s = Segmenter::new(130.0);
        let mut t = 0;
        let mut feed = |level: f32, secs: f32, s: &mut Segmenter| {
            let mut out = Vec::new();
            for _ in 0..(secs / 0.032) as usize {
                if let Some(u) = s.push(&chunk(level, 512, t)) {
                    out.push(u);
                }
                t += 512;
            }
            out
        };
        assert!(feed(130.0, 2.0, &mut s).is_empty());
        assert!(feed(400.0, 1.5, &mut s).is_empty());
        let got = feed(130.0, 1.2, &mut s);
        assert_eq!(got.len(), 1);
        let secs = got[0].len() as f32 / RATE;
        assert!((1.5..3.0).contains(&secs), "{secs}");
        // A click is not speech.
        assert!(feed(600.0, 0.1, &mut s).is_empty());
        assert!(feed(130.0, 1.2, &mut s).is_empty());
        // The robot speaking mutes the mic (zeros): ends an utterance, never starts one.
        assert!(feed(0.0, 1.0, &mut s).is_empty());
    }
}
