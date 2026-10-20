//! Face recognition in AI mode, on the laptop.
//!
//! While the robot streams video, a few frames a second go through face detection and
//! recognition. People enrolled by name raise `face` events when they arrive or leave, show up
//! in `status`, and get a spoken hello (at most once per `greet_cooldown_secs`). Nothing runs,
//! and the models are not even downloaded, until someone is enrolled or `who_is_here` is used.

pub mod db;
pub mod engine;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bbproto::frame::VideoFrame;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{OnceCell, broadcast::error::RecvError};
use tokio::time::Instant;

use crate::config::FacesConfig;
use crate::control::Robot;
use crate::hub::Hub;
use db::FaceDb;
use engine::{Detection, Engine};

/// Analyse at most one frame per this interval.
const PERIOD: Duration = Duration::from_millis(300);
/// Faces analysed per frame (the largest ones).
const MAX_FACES: usize = 3;
/// Consecutive analysed frames a person must appear in before they count as arrived.
const ARRIVE_HITS: u32 = 2;
/// Someone not seen for this long has left.
const LEAVE_AFTER: Duration = Duration::from_secs(6);
/// `in_view` older than this is stale.
const VIEW_FRESH: Duration = Duration::from_secs(2);
/// After a failed model load (usually the download), wait this long before trying again.
const LOAD_RETRY: Duration = Duration::from_secs(60);
/// Enrollment: samples wanted, the minimum accepted, and how long to look.
const ENROLL_SAMPLES: usize = 8;
const ENROLL_MIN_SAMPLES: usize = 3;
const ENROLL_TIME: Duration = Duration::from_secs(12);
/// Smallest face width (pixels in the camera frame) accepted for enrollment.
const ENROLL_MIN_WIDTH: f32 = 28.0;
/// Faces narrower than this (frame pixels) are too blurry to name reliably; they are reported as
/// unknown.
const RECOGNIZE_MIN_WIDTH: f32 = 20.0;
/// An enrollment sample whose median similarity to the others is below this is a bad frame
/// (blur, a turned head) and is dropped.
const ENROLL_CONSISTENCY: f32 = 0.4;
/// Someone who talked to the robot this recently is not greeted as if they just arrived.
const TALKED_RECENTLY: Duration = Duration::from_secs(600);
/// Track key for faces that match nobody.
const UNKNOWN: &str = "";

/// A face in the latest analysed frame.
#[derive(Debug, Clone, Serialize)]
pub struct Seen {
    /// Enrolled name, or null for someone unknown.
    pub name: Option<String>,
    /// Similarity to the enrolled person (0..1).
    pub score: Option<f32>,
    /// x, y, width, height in frame pixels.
    pub bbox: [f32; 4],
    pub frame_width: u16,
    pub frame_height: u16,
}

#[derive(Default)]
struct Track {
    hits: u32,
    present: bool,
    last_seen: Option<Instant>,
    /// When they were last in view before they left.
    left_at: Option<Instant>,
    last_greeted: Option<Instant>,
}

/// Someone came into view or left it.
struct Change {
    key: String,
    arrived: bool,
    score: Option<f32>,
    /// How long they had been out of view (None: not seen before).
    away: Option<Duration>,
}

#[derive(Default)]
struct View {
    faces: Vec<Seen>,
    at: Option<Instant>,
    tracks: HashMap<String, Track>,
}

pub struct Faces {
    hub: Arc<Hub>,
    cfg: FacesConfig,
    models_dir: PathBuf,
    db_path: PathBuf,
    engine: OnceCell<Arc<Engine>>,
    /// When and why loading the models last failed.
    load_failure: Mutex<Option<(Instant, String)>>,
    db: Mutex<FaceDb>,
    view: Mutex<View>,
}

/// Result of `remember`.
#[derive(Debug, Serialize)]
pub struct Enrolled {
    pub name: String,
    pub samples_added: usize,
    /// Someone else already enrolled who these samples also resemble.
    pub resembles: Option<String>,
}

impl Faces {
    pub fn new(hub: Arc<Hub>) -> Faces {
        let data = hub.config.data_dir();
        let db_path = FaceDb::path(&data);
        let db = FaceDb::load(&db_path).unwrap_or_else(|e| {
            // Keep the unreadable file for inspection rather than overwriting it later.
            let bad = db_path.with_extension("json.bad");
            tracing::error!("{e:#}; moving it to {} and starting empty", bad.display());
            let _ = std::fs::rename(&db_path, &bad);
            FaceDb::default()
        });
        Faces {
            cfg: hub.config.faces.clone(),
            models_dir: data.join("models"),
            db_path,
            engine: OnceCell::new(),
            load_failure: Mutex::new(None),
            db: Mutex::new(db),
            view: Mutex::new(View::default()),
            hub,
        }
    }

    pub fn known(&self) -> Vec<String> {
        self.db.lock().unwrap().names()
    }

    /// Faces seen in the latest analysed frame (empty if that is stale).
    pub fn in_view(&self) -> Vec<Seen> {
        let v = self.view.lock().unwrap();
        match v.at {
            Some(at) if at.elapsed() < VIEW_FRESH => v.faces.clone(),
            _ => Vec::new(),
        }
    }

    pub fn status_json(&self) -> Value {
        json!({
            "enabled": self.cfg.enabled,
            "known": self.known(),
            "in_view": self.in_view(),
        })
    }

    /// The models, loading (and on first use downloading, ~40 MB) them if needed.
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
                    .context("loading the face recognition models")
            })
            .await
            .cloned();
        if let Err(e) = &r {
            *self.load_failure.lock().unwrap() = Some((Instant::now(), format!("{e:#}")));
        }
        r
    }

    /// Detect faces in a frame and embed the largest few.
    async fn analyze(&self, frame: &VideoFrame) -> Result<Vec<(Detection, Vec<f32>)>> {
        let engine = self.engine().await?;
        let jpeg = frame.jpeg.clone();
        tokio::task::spawn_blocking(move || {
            let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                .context("decoding camera frame")?
                .to_rgb8();
            let mut dets = engine.detect(&img)?;
            dets.sort_by(|a, b| (b.bbox[2] * b.bbox[3]).total_cmp(&(a.bbox[2] * a.bbox[3])));
            dets.truncate(MAX_FACES);
            dets.into_iter()
                .map(|d| {
                    let e = engine.embed(&img, &d)?;
                    Ok((d, e))
                })
                .collect()
        })
        .await
        .map_err(|e| anyhow!("face analysis panicked: {e}"))?
    }

    /// Analyse one frame, update who is in view, and return arrivals and departures.
    async fn observe(&self, frame: &VideoFrame) -> Result<Vec<Change>> {
        let faces = self.analyze(frame).await?;
        let seen: Vec<Seen> = {
            let db = self.db.lock().unwrap();
            faces
                .iter()
                .map(|(d, e)| {
                    if tracing::enabled!(tracing::Level::DEBUG) {
                        let closest = db.best_match(e, -1.0);
                        tracing::debug!(
                            "face {:.0}x{:.0} at ({:.0},{:.0}) det {:.2}, closest {:?}",
                            d.bbox[2],
                            d.bbox[3],
                            d.bbox[0],
                            d.bbox[1],
                            d.score,
                            closest
                        );
                    }
                    let m = self.name(&db, d, e);
                    Seen {
                        name: m.as_ref().map(|m| m.name.clone()),
                        score: m.map(|m| m.score),
                        bbox: d.bbox,
                        frame_width: frame.width,
                        frame_height: frame.height,
                    }
                })
                .collect()
        };
        let now = Instant::now();
        let mut v = self.view.lock().unwrap();
        let mut changes = Vec::new();
        let mut present_now: HashMap<String, Option<f32>> = HashMap::new();
        for s in &seen {
            let key = s.name.clone().unwrap_or_else(|| UNKNOWN.to_string());
            let best = present_now.entry(key).or_insert(s.score);
            if s.score > *best {
                *best = s.score;
            }
        }
        for (key, score) in &present_now {
            let t = v.tracks.entry(key.clone()).or_default();
            t.hits += 1;
            t.last_seen = Some(now);
            if !t.present && t.hits >= ARRIVE_HITS {
                t.present = true;
                changes.push(Change {
                    key: key.clone(),
                    arrived: true,
                    score: *score,
                    away: t.left_at.map(|l| now - l),
                });
            }
        }
        for (key, t) in v.tracks.iter_mut() {
            if present_now.contains_key(key) {
                continue;
            }
            t.hits = 0;
            if t.present && t.last_seen.is_none_or(|s| now - s > LEAVE_AFTER) {
                t.present = false;
                t.left_at = t.last_seen;
                changes.push(Change {
                    key: key.clone(),
                    arrived: false,
                    score: None,
                    away: None,
                });
            }
        }
        v.faces = seen;
        v.at = Some(now);
        Ok(changes)
    }

    fn name(&self, db: &FaceDb, d: &Detection, embedding: &[f32]) -> Option<db::Match> {
        if d.bbox[2] < RECOGNIZE_MIN_WIDTH {
            return None;
        }
        db.best_match(embedding, self.cfg.threshold)
    }

    /// Everyone still marked present is reported gone: the video stopped (AI mode ended, link
    /// lost), so nobody can be seen. Returns their track keys.
    fn sweep(&self) -> Vec<String> {
        let mut v = self.view.lock().unwrap();
        v.faces.clear();
        v.tracks
            .iter_mut()
            .filter_map(|(k, t)| {
                t.hits = 0;
                if t.present {
                    t.left_at = t.last_seen;
                }
                std::mem::take(&mut t.present).then(|| k.clone())
            })
            .collect()
    }

    /// Mark `name` as greeted now if a greeting is due.
    fn take_greeting(&self, name: &str) -> bool {
        if !self.cfg.greet || name == UNKNOWN {
            return false;
        }
        let cooldown = Duration::from_secs(self.cfg.greet_cooldown_secs);
        let mut v = self.view.lock().unwrap();
        let t = v.tracks.entry(name.to_string()).or_default();
        if t.last_greeted.is_some_and(|g| g.elapsed() < cooldown) {
            return false;
        }
        t.last_greeted = Some(Instant::now());
        true
    }

    /// Who is in front of the camera now: the watcher's latest result, or a fresh look.
    pub async fn who_is_here(&self, robot: &Robot) -> Result<Vec<Seen>> {
        if !self.cfg.enabled {
            bail!("face recognition is off ([faces] enabled = false)");
        }
        if self
            .view
            .lock()
            .unwrap()
            .at
            .is_some_and(|at| at.elapsed() < VIEW_FRESH)
        {
            return Ok(self.in_view());
        }
        // A frame that arrives from now on: latest_frame may predate a camera change.
        robot.ensure_remote().await?;
        let mut rx = self.hub.video.subscribe();
        let frame = match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Ok(f)) => (*f).clone(),
            _ => {
                self.hub
                    .snapshot(self.hub.config.camera.res, self.hub.config.camera.quality)
                    .await?
            }
        };
        let faces = self.analyze(&frame).await?;
        let db = self.db.lock().unwrap();
        Ok(faces
            .iter()
            .map(|(d, e)| {
                let m = self.name(&db, d, e);
                Seen {
                    name: m.as_ref().map(|m| m.name.clone()),
                    score: m.map(|m| m.score),
                    bbox: d.bbox,
                    frame_width: frame.width,
                    frame_height: frame.height,
                }
            })
            .collect())
    }

    /// Enroll the one person in front of the camera as `name`, from several live frames.
    pub async fn remember(&self, robot: &Robot, name: &str) -> Result<Enrolled> {
        if !self.cfg.enabled {
            bail!("face recognition is off ([faces] enabled = false)");
        }
        // Validate the name before spending time on the camera.
        let name = &db::clean_name(name)?;
        robot.ensure_remote().await?;
        self.engine().await?;
        let mut rx = self.hub.video.subscribe();
        let deadline = Instant::now() + ENROLL_TIME;
        let mut samples = Vec::new();
        let (mut none, mut several, mut small) = (0, 0, 0);
        let mut last = None::<Instant>;
        while samples.len() < ENROLL_SAMPLES {
            let frame = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(f)) => f,
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => bail!("video stream closed"),
                Err(_) => break,
            };
            if last.is_some_and(|l| l.elapsed() < PERIOD) {
                continue;
            }
            last = Some(Instant::now());
            let faces = self.analyze(&frame).await?;
            match faces.as_slice() {
                [] => none += 1,
                [(d, e)] if d.bbox[2] >= ENROLL_MIN_WIDTH => samples.push(e.clone()),
                [_] => small += 1,
                _ => several += 1,
            }
        }
        let collected = samples.len();
        let samples = consistent(samples, ENROLL_CONSISTENCY);
        if collected >= ENROLL_MIN_SAMPLES && samples.len() < ENROLL_MIN_SAMPLES {
            bail!(
                "the {collected} frames did not agree with each other; hold still, face the camera \
in good light, and make sure only one person is in front of the robot"
            );
        }
        if samples.len() < ENROLL_MIN_SAMPLES {
            let why = if several >= none.max(small) && several > 0 {
                "more than one face was in view; make sure only the person to remember is in front of the robot"
            } else if small >= none && small > 0 {
                "the face was too small; come closer (about an arm's length) and face the camera"
            } else {
                "no face was found; face the camera from about an arm's length, in good light"
            };
            bail!(
                "could not get a clear look ({} good frames): {why}",
                samples.len()
            );
        }
        let resembles = {
            let db = self.db.lock().unwrap();
            samples
                .iter()
                .filter_map(|s| db.best_match(s, self.cfg.threshold))
                .find(|m| !m.name.eq_ignore_ascii_case(name))
                .map(|m| m.name)
        };
        let added = samples.len();
        let enrolled_name = {
            let mut db = self.db.lock().unwrap();
            db.enroll(name, samples)?;
            db.save(&self.db_path)?;
            db.names()
                .into_iter()
                .find(|n| n.eq_ignore_ascii_case(name))
                .unwrap_or_else(|| name.clone())
        };
        // They are standing right there: no greeting for the next while.
        {
            let mut v = self.view.lock().unwrap();
            let t = v.tracks.entry(enrolled_name.clone()).or_default();
            t.last_greeted = Some(Instant::now());
        }
        self.hub
            .push_event(
                "face",
                json!({ "event": "enrolled", "name": enrolled_name }),
            )
            .await;
        Ok(Enrolled {
            name: enrolled_name,
            samples_added: added,
            resembles,
        })
    }

    pub async fn forget(&self, name: &str) -> Result<bool> {
        let name = &db::normalize_name(name);
        let removed = {
            let mut db = self.db.lock().unwrap();
            let removed = db.forget(name);
            if removed {
                db.save(&self.db_path)?;
            }
            removed
        };
        if removed {
            self.view
                .lock()
                .unwrap()
                .tracks
                .retain(|k, _| !k.eq_ignore_ascii_case(name));
        }
        Ok(removed)
    }
}

/// The samples that resemble the rest of the batch (median similarity at least `min`).
fn consistent(samples: Vec<Vec<f32>>, min: f32) -> Vec<Vec<f32>> {
    let medians: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let mut v: Vec<f32> = samples
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, b)| engine::similarity_score(a, b))
                .collect();
            v.sort_by(f32::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(1.0)
        })
        .collect();
    samples
        .into_iter()
        .zip(medians)
        .filter(|(_, m)| *m >= min)
        .map(|(s, _)| s)
        .collect()
}

/// Watch the live video while the robot streams it. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let faces = robot.faces.clone();
    let hub = robot.hub.clone();
    let mut rx = hub.video.subscribe();
    let mut last = Instant::now() - PERIOD;
    let mut last_error = String::new();
    loop {
        match tokio::time::timeout(LEAVE_AFTER, rx.recv()).await {
            Ok(Ok(_)) => {}
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => return,
            Err(_) => {
                // No video for a while: whoever was here is gone as far as anyone can tell.
                for key in faces.sweep() {
                    let name = (key != UNKNOWN).then_some(key);
                    hub.push_event("face", json!({ "event": "left", "name": name }))
                        .await;
                }
                continue;
            }
        }
        if !faces.cfg.enabled || faces.db.lock().unwrap().is_empty() || last.elapsed() < PERIOD {
            continue;
        }
        last = Instant::now();
        // Work on the newest frame, not whichever one woke us.
        let Some(frame) = hub.latest_frame().await else {
            continue;
        };
        let changes = match faces.observe(&frame).await {
            Ok(c) => {
                last_error.clear();
                c
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg != last_error {
                    tracing::warn!("face recognition: {msg}");
                    last_error = msg;
                }
                continue;
            }
        };
        // Do not talk over the robot (or the AI) speaking; those greetings are skipped, not queued.
        let quiet = !hub.status.lock().await.speaker_playing;
        let mut greet_names = Vec::new();
        let welcome_back = Duration::from_secs(faces.cfg.welcome_back_minutes * 60);
        for Change {
            key,
            arrived,
            score,
            away,
        } in changes
        {
            let name = (key != UNKNOWN).then_some(key.clone());
            // Back after a while, and not just talking with the robot (the low camera often
            // misses people who are right there).
            let welcome = arrived
                && key != UNKNOWN
                && away.is_none_or(|a| a >= welcome_back)
                && !robot.ears.talked_within(TALKED_RECENTLY);
            if welcome {
                robot.life.arrived(&key);
            }
            // With an agent running, it greets people itself (it gets an "arrived" nudge).
            let agent = robot
                .life
                .agent_running
                .load(std::sync::atomic::Ordering::Relaxed);
            let greet = welcome && quiet && !agent && faces.take_greeting(&key);
            if greet {
                greet_names.push(key.clone());
            }
            hub.push_event(
                "face",
                json!({
                    "event": if arrived { "arrived" } else { "left" },
                    "name": name,
                    "score": score,
                    "greeted": greet,
                }),
            )
            .await;
        }
        // One utterance for everyone who arrived together; separate ones would cut each other off.
        if !greet_names.is_empty() {
            let text = format!("Hi, {}!", join_names(&greet_names));
            let robot = robot.clone();
            tokio::spawn(async move {
                if let Err(e) = robot.say(&text, Some("robot"), false).await {
                    tracing::warn!("greeting failed: {e:#}");
                }
            });
        }
    }
}

/// "A", "A and B", "A, B and C".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    #[test]
    fn joins_names() {
        let n = |v: &[&str]| join_names(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["A"]), "A");
        assert_eq!(n(&["A", "B"]), "A and B");
        assert_eq!(n(&["A", "B", "C"]), "A, B and C");
    }

    #[test]
    fn drops_samples_unlike_the_rest() {
        let good = |d: f32| unit(&[1.0, d, 0.0]);
        let samples = vec![
            good(0.0),
            good(0.1),
            good(-0.1),
            good(0.2),
            unit(&[0.0, 0.0, 1.0]),
        ];
        let kept = consistent(samples, 0.4);
        assert_eq!(kept.len(), 4);
        assert!(kept.iter().all(|s| s[2] == 0.0));
    }
}
