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
/// After the robot's name alone ("Butterbot." then a pause), the next utterance within this
/// long is the command.
const NAME_ONLY_WINDOW: Duration = Duration::from_secs(8);
/// Silero voice activity detection works on 32 ms frames, the robot's chunk size.
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const FRAME: usize = 512;
/// Audio kept and analysed at a time (also the lead-in before detected speech): 1 s.
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const WINDOW: usize = 16_000;
/// Run the detector every 128 ms.
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const CHECK_EVERY: usize = 4 * FRAME;
/// Speech starts when at least START_FRAMES of the last 8 frames are above START_PROB...
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const START_PROB: f32 = 0.5;
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const START_FRAMES: usize = 4;
/// ...and ends after END_FRAMES (0.5 s) in a row below END_PROB.
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const END_PROB: f32 = 0.35;
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const END_FRAMES: usize = 16;
/// Utterances with fewer speech frames (about 0.2 s) are coughs and clicks.
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const MIN_SPEECH_FRAMES: usize = 6;
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
const MAX_UTTERANCE: usize = 12 * 16_000;
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
    /// Who it was, by voice, if it is a voice the robot knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
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
    /// Called by name alone: the next utterance until then is the command, even in focus mode.
    name_until: Mutex<Option<Instant>>,
    /// Off in focus mode: only speech addressed by name gets through, no follow-up window.
    attentive: AtomicBool,
    /// When anyone last spoke near the robot (to it or not): someone is around.
    last_speech: Mutex<Option<Instant>>,
    /// The last command handed to the agent, for sanity checks such as `power_off`.
    last_command_text: Mutex<String>,
    /// The conversation was ended ("goodbye"): no follow-up window until the next command.
    ended: AtomicBool,
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
            name_until: Mutex::new(None),
            attentive: AtomicBool::new(true),
            last_speech: Mutex::new(None),
            last_command_text: Mutex::new(String::new()),
            ended: AtomicBool::new(false),
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
        self.start_listening().await?;
        let deadline = Instant::now() + timeout;
        loop {
            let arrived = self.arrived.notified();
            if let Some(c) = self.take_command() {
                return Ok(Some(c));
            }
            if tokio::time::timeout_at(deadline, arrived).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// Turn on listening (and load the speech model) if it is not on yet.
    pub async fn start_listening(&self) -> Result<()> {
        if !self.cfg.enabled {
            bail!("speech-to-text is off ([stt] enabled = false)");
        }
        if !self.active.swap(true, Ordering::Relaxed) {
            tracing::info!("listening for \"{}, ...\"", self.cfg.wake_word);
        }
        #[cfg(feature = "stt")]
        self.engine().await?;
        Ok(())
    }

    /// The oldest command still fresh enough to answer, if any.
    pub fn take_command(&self) -> Option<Command> {
        let max_age = Duration::from_secs(self.cfg.command_max_age_secs);
        let mut q = self.commands.lock().unwrap();
        while let Some(c) = q.pop_front() {
            if c.heard_at.is_some_and(|t| t.elapsed() <= max_age) {
                *self.last_command_text.lock().unwrap() = c.text.clone();
                return Some(c);
            }
        }
        None
    }

    /// Whether the last command asked the robot to switch itself off. Models have taken a
    /// plain "goodbye" for it.
    pub fn asked_to_power_off(&self) -> bool {
        let t = self.last_command_text.lock().unwrap().to_lowercase();
        [
            "turn off",
            "turn yourself off",
            "switch off",
            "switch yourself off",
            "shut down",
            "shutdown",
            "power off",
            "power down",
            "turn it off",
            "go to sleep",
        ]
        .iter()
        .any(|p| t.contains(p))
    }

    /// Wait up to `d` for a command to arrive (it is then fetched with `take_command`).
    pub async fn wait_command(&self, d: Duration) {
        let arrived = self.arrived.notified();
        if self.commands.lock().unwrap().is_empty() {
            let _ = tokio::time::timeout(d, arrived).await;
        }
    }

    /// Treat what is said next (within the name-only window) as a command, as if called by name:
    /// the controller's Listen button.
    pub fn listen_now(&self) {
        self.ended.store(false, Ordering::Relaxed);
        self.called_by_name(NAME_ONLY_WINDOW);
    }

    /// A conversation starts without a spoken command (the controller's Poke): the robot's
    /// answer opens the follow-up window.
    pub fn conversation_opened(&self) {
        self.ended.store(false, Ordering::Relaxed);
        *self.last_command.lock().unwrap() = Some(Instant::now());
    }

    /// A command that did not come from the microphone (the controller's menu).
    pub fn command(&self, text: &str) {
        self.push(Command {
            text: text.to_string(),
            said: text.to_string(),
            follow_up: false,
            at_ms: crate::hub::now_ms(),
            in_view: Vec::new(),
            speaker: None,
            heard_at: Some(Instant::now()),
        });
    }

    /// Whether anyone has given the robot a command within `d`.
    pub fn talked_within(&self, d: Duration) -> bool {
        self.last_command
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < d)
    }

    /// Whether anyone has spoken near the robot within `d`.
    pub fn heard_speech_within(&self, d: Duration) -> bool {
        self.last_speech
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < d)
    }

    /// In focus mode only speech that starts with the robot's name gets through.
    pub fn set_attentive(&self, on: bool) {
        self.attentive.store(on, Ordering::Relaxed);
        if !on {
            *self.open_until.lock().unwrap() = None;
        }
    }

    /// Whether speech that started at `at` counts as a command without the wake word. The
    /// window only has to be open when someone starts talking, not when they finish.
    fn is_open_at(&self, at: Instant) -> bool {
        if self
            .name_until
            .lock()
            .unwrap()
            .take_if(|t| at < *t)
            .is_some()
        {
            return true;
        }
        self.attentive.load(Ordering::Relaxed)
            && self.open_until.lock().unwrap().is_some_and(|t| at < t)
    }

    /// Whether speech now counts as a command without the wake word. Unlike `is_open` it does not
    /// use up a call by name (for the status light).
    pub fn window_open(&self) -> bool {
        let now = Instant::now();
        self.name_until.lock().unwrap().is_some_and(|t| now < t)
            || (self.attentive.load(Ordering::Relaxed)
                && self.open_until.lock().unwrap().is_some_and(|t| now < t))
    }

    /// The robot heard its name alone: what is said next (within `d`) is the command.
    fn called_by_name(&self, d: Duration) {
        *self.name_until.lock().unwrap() = Some(Instant::now() + d);
    }

    /// Treat whatever is said in the next `d` as addressed to the robot.
    fn open_for(&self, d: Duration) {
        if !self.attentive.load(Ordering::Relaxed) {
            return;
        }
        let until = Instant::now() + d;
        let mut o = self.open_until.lock().unwrap();
        if o.is_none_or(|t| t < until) {
            *o = Some(until);
        }
    }

    /// End the conversation: from now on only speech that starts with the robot's name is for
    /// it (until the next such command). Its goodbye does not reopen the window.
    pub fn end_conversation(&self) {
        self.ended.store(true, Ordering::Relaxed);
        *self.open_until.lock().unwrap() = None;
        *self.name_until.lock().unwrap() = None;
    }

    /// The robot stopped speaking: if that was (likely) an answer, keep listening for a reply.
    fn speech_ended(&self) {
        let recent = self
            .last_command
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < CONVERSATION);
        if recent && !self.ended.load(Ordering::Relaxed) {
            self.open_for(Duration::from_secs(self.cfg.follow_up_secs));
        }
    }

    fn push(&self, c: Command) {
        *self.last_command.lock().unwrap() = Some(Instant::now());
        // "Goodbye" ends the conversation; anything else (re)starts one.
        let bye = crate::stt::is_goodbye(&c.text);
        self.ended.store(bye, Ordering::Relaxed);
        if bye {
            *self.open_until.lock().unwrap() = None;
            let mut q = self.commands.lock().unwrap();
            q.push_back(c);
            drop(q);
            self.arrived.notify_waiters();
            return;
        }
        // Also open the window now, in case the agent answers without speaking.
        self.open_for(Duration::from_secs(self.cfg.follow_up_secs));
        let mut q = self.commands.lock().unwrap();
        q.push_back(c);
        while q.len() > QUEUE_LEN {
            q.pop_front();
        }
        drop(q);
        self.arrived.notify_waiters();
    }
}

/// Speech start and end from Silero's per-frame speech probabilities.
#[cfg_attr(not(feature = "stt"), allow(dead_code))]
#[derive(Default)]
struct Tracker {
    in_speech: bool,
    speech_frames: usize,
}

#[cfg_attr(not(feature = "stt"), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum Edge {
    None,
    Start,
    /// `keep`: enough speech to be worth transcribing.
    End {
        keep: bool,
    },
}

#[cfg_attr(not(feature = "stt"), allow(dead_code))]
impl Tracker {
    /// `probs`: speech probability per frame over the analysed window, oldest first; the last
    /// `new_frames` of them arrived since the previous call.
    fn update(&mut self, probs: &[f32], new_frames: usize) -> Edge {
        let tail = |n: usize| &probs[probs.len().saturating_sub(n)..];
        let speechy = |ps: &[f32]| ps.iter().filter(|p| **p > START_PROB).count();
        if !self.in_speech {
            let n = speechy(tail(8));
            if n >= START_FRAMES {
                self.in_speech = true;
                self.speech_frames = n;
                return Edge::Start;
            }
            return Edge::None;
        }
        self.speech_frames += speechy(tail(new_frames));
        if probs.len() >= END_FRAMES && tail(END_FRAMES).iter().all(|p| *p < END_PROB) {
            self.in_speech = false;
            return Edge::End {
                keep: self.speech_frames >= MIN_SPEECH_FRAMES,
            };
        }
        Edge::None
    }
}

/// Silero voice activity detection (via whisper.cpp), downloaded on first use.
#[cfg(feature = "stt")]
const VAD_MODEL: crate::models::ModelFile = crate::models::ModelFile {
    name: "ggml-silero-v5.1.2.bin",
    url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin",
    sha256: "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf",
    max_bytes: 4 << 20,
};

/// Cuts the microphone stream into utterances with the voice activity detector, on its own
/// thread (the detector is not `Send`). Music, TV and room noise do not count as speech.
#[cfg(feature = "stt")]
fn vad_thread(
    dir: PathBuf,
    chunks: std::sync::mpsc::Receiver<Vec<i16>>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
) {
    use whisper_rs::{WhisperVadContext, WhisperVadContextParams};
    let mut vad = match crate::models::fetch(&dir, &VAD_MODEL).and_then(|p| {
        let mut params = WhisperVadContextParams::new();
        params.set_n_threads(1);
        WhisperVadContext::new(&p.to_string_lossy(), params)
            .map_err(|e| anyhow::anyhow!("loading the voice activity model: {e}"))
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("listening is off: {e:#}");
            return;
        }
    };
    let mut window: VecDeque<i16> = VecDeque::with_capacity(WINDOW + FRAME);
    let mut since = 0;
    let mut tracker = Tracker::default();
    let mut utterance: Vec<i16> = Vec::new();
    while let Ok(chunk) = chunks.recv() {
        // All zeros: the robot muted its microphone because it is speaking.
        if chunk.iter().all(|&s| s == 0) {
            if tracker.in_speech && tracker.speech_frames >= MIN_SPEECH_FRAMES {
                let _ = out.send(std::mem::take(&mut utterance));
            }
            tracker = Tracker::default();
            utterance.clear();
            window.clear();
            since = 0;
            continue;
        }
        window.extend(chunk.iter().copied());
        while window.len() > WINDOW {
            window.pop_front();
        }
        if tracker.in_speech {
            utterance.extend_from_slice(&chunk);
            if utterance.len() >= MAX_UTTERANCE {
                tracker = Tracker::default();
                let _ = out.send(std::mem::take(&mut utterance));
                continue;
            }
        }
        since += chunk.len();
        if since < CHECK_EVERY {
            continue;
        }
        let new_frames = since / FRAME;
        since = 0;
        let samples: Vec<f32> = window.iter().map(|&s| s as f32 / 32_768.0).collect();
        if let Err(e) = vad.detect_speech(&samples) {
            tracing::debug!("voice activity: {e}");
            continue;
        }
        match tracker.update(vad.probabilities(), new_frames) {
            // Start from the whole window: the lead-in holds the first syllables.
            Edge::Start => utterance = window.iter().copied().collect(),
            Edge::End { keep } => {
                let u = std::mem::take(&mut utterance);
                if keep {
                    let _ = out.send(u);
                }
            }
            Edge::None => {}
        }
    }
}

/// Watch the microphone while the robot streams it. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let ears = robot.ears.clone();
    let hub = robot.hub.clone();
    let mut rx = hub.mic.subscribe();
    let (utterance_tx, mut utterances) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
    #[cfg(feature = "stt")]
    let mut vad: Option<std::sync::mpsc::Sender<Vec<i16>>> = None;
    #[cfg(not(feature = "stt"))]
    let _ = &utterance_tx; // nothing feeds utterances without speech-to-text
    let mut speaking = false;
    loop {
        let chunk = tokio::select! {
            r = rx.recv() => match r {
                Ok(c) => c,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            Some(u) = utterances.recv() => {
                handle_utterance(ears.clone(), robot.clone(), u);
                continue;
            }
        };
        let now_speaking = hub.status.lock().await.speaker_playing;
        if speaking && !now_speaking {
            ears.speech_ended();
        }
        speaking = now_speaking;
        if !ears.cfg.enabled || !ears.active.load(Ordering::Relaxed) {
            #[cfg(feature = "stt")]
            {
                vad = None;
            }
            continue;
        }
        #[cfg(not(feature = "stt"))]
        let _ = chunk;
        #[cfg(feature = "stt")]
        {
            let tx = vad.get_or_insert_with(|| {
                let (tx, rx) = std::sync::mpsc::channel();
                let dir = ears.models_dir.clone();
                let out = utterance_tx.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("ears-vad".into())
                    .spawn(move || vad_thread(dir, rx, out))
                {
                    tracing::error!("starting the listener: {e}");
                }
                tx
            });
            if tx.send(chunk.to_vec()).is_err() {
                vad = None;
            }
        }
    }
}

/// Transcribe an utterance and, if it is for the robot, queue it as a command.
fn handle_utterance(ears: Arc<Ears>, robot: Robot, utterance: Vec<i16>) {
    let secs = utterance.len() as f32 / RATE;
    // When the person started talking (the utterance ends about now).
    let spoke_at = Instant::now()
        .checked_sub(Duration::from_secs_f32(secs))
        .unwrap_or_else(Instant::now);
    tokio::spawn(async move {
        let started = Instant::now();
        // Kept for the voiceprint, only if it turns out to be for the robot.
        let pcm = utterance.clone();
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
        *ears.last_speech.lock().unwrap() = Some(Instant::now());
        let (text, follow_up) = match crate::stt::addressed(&said, &ears.cfg.wake_word) {
            // Just the name, then (probably) a pause: the command comes next.
            Some("") => {
                tracing::info!("called by name; listening for the command");
                ears.called_by_name(NAME_ONLY_WINDOW);
                return;
            }
            Some(t) => (t.to_string(), false),
            None if ears.is_open_at(spoke_at) => (said.clone(), true),
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
        let speaker = robot.voices.hear(pcm, &in_view).await;
        if let Some((name, score)) = &speaker {
            tracing::info!("voice: {name} ({score:.2})");
        }
        let speaker = speaker.map(|(name, _)| name);
        let c = Command {
            text,
            said: said.clone(),
            follow_up,
            at_ms: now_ms(),
            in_view: in_view.clone(),
            speaker: speaker.clone(),
            heard_at: Some(Instant::now()),
        };
        robot
            .hub
            .push_event(
                "heard",
                json!({ "text": c.text, "follow_up": follow_up, "in_view": in_view, "speaker": speaker }),
            )
            .await;
        ears.push(c);
        robot.light.thinking();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_speech_from_probabilities() {
        let mut t = Tracker::default();
        let mut probs = vec![0.1f32; 31];
        assert_eq!(t.update(&probs, 4), Edge::None);
        // A burst of speech.
        probs.drain(..4);
        probs.extend([0.9, 0.8, 0.95, 0.7]);
        assert_eq!(t.update(&probs, 4), Edge::Start);
        probs.drain(..4);
        probs.extend([0.9; 4]);
        assert_eq!(t.update(&probs, 4), Edge::None);
        // Quiet for less than 0.5 s: still speaking (a pause between words).
        probs.drain(..8);
        probs.extend([0.1; 8]);
        assert_eq!(t.update(&probs, 8), Edge::None);
        probs.drain(..8);
        probs.extend([0.1; 8]);
        assert_eq!(t.update(&probs, 8), Edge::End { keep: true });

        // A single loud frame is not speech; a short blip is dropped.
        let mut t = Tracker::default();
        let mut p = vec![0.1f32; 31];
        p[30] = 0.9;
        assert_eq!(t.update(&p, 4), Edge::None);
        p[27..31].copy_from_slice(&[0.9; 4]);
        assert_eq!(t.update(&p, 4), Edge::Start);
        let p = vec![0.1f32; 31];
        assert_eq!(t.update(&p, 16), Edge::End { keep: false });
    }
}
