//! Face recognition in AI mode, on the laptop.
//!
//! While the robot streams video, a few frames a second go through face detection and
//! recognition. People enrolled by name raise `face` events when they arrive or leave, show up
//! in `status`, and get a spoken hello (at most once per `greet_cooldown_secs`). Nothing runs,
//! and the models are not even downloaded, until someone is enrolled.

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
/// Enrollment: samples wanted, the minimum accepted, and how long to look.
const ENROLL_SAMPLES: usize = 8;
const ENROLL_MIN_SAMPLES: usize = 3;
const ENROLL_TIME: Duration = Duration::from_secs(12);
/// Smallest face width (pixels in the camera frame) accepted for enrollment.
const ENROLL_MIN_WIDTH: f32 = 28.0;
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
    last_greeted: Option<Instant>,
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
        let dir = self.models_dir.clone();
        self.engine
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || Engine::load(&dir).map(Arc::new))
                    .await
                    .map_err(|e| anyhow!("model loader panicked: {e}"))?
                    .context("loading the face recognition models")
            })
            .await
            .cloned()
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
    async fn observe(&self, frame: &VideoFrame) -> Result<Vec<(String, bool, Option<f32>)>> {
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
                    let m = db.best_match(e, self.cfg.threshold);
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
                changes.push((key.clone(), true, *score));
            }
        }
        for (key, t) in v.tracks.iter_mut() {
            if present_now.contains_key(key) {
                continue;
            }
            t.hits = 0;
            if t.present && t.last_seen.is_none_or(|s| now - s > LEAVE_AFTER) {
                t.present = false;
                changes.push((key.clone(), false, None));
            }
        }
        v.faces = seen;
        v.at = Some(now);
        Ok(changes)
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
        if self
            .view
            .lock()
            .unwrap()
            .at
            .is_some_and(|at| at.elapsed() < VIEW_FRESH)
        {
            return Ok(self.in_view());
        }
        let frame = robot.snapshot(None, 12).await?;
        let faces = self.analyze(&frame).await?;
        let db = self.db.lock().unwrap();
        Ok(faces
            .iter()
            .map(|(d, e)| {
                let m = db.best_match(e, self.cfg.threshold);
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
        FaceDb::default().enroll(name, Vec::new())?;
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
                .find(|m| !m.name.eq_ignore_ascii_case(name.trim()))
                .map(|m| m.name)
        };
        let added = samples.len();
        let enrolled_name = {
            let mut db = self.db.lock().unwrap();
            db.enroll(name, samples)?;
            db.save(&self.db_path)?;
            db.names()
                .into_iter()
                .find(|n| n.eq_ignore_ascii_case(name.trim()))
                .unwrap_or_else(|| name.trim().to_string())
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
                .retain(|k, _| !k.eq_ignore_ascii_case(name.trim()));
        }
        Ok(removed)
    }
}

/// Watch the live video while the robot streams it. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let faces = robot.faces.clone();
    let hub = robot.hub.clone();
    let mut rx = hub.video.subscribe();
    let mut last = Instant::now() - PERIOD;
    let mut last_error = String::new();
    loop {
        match rx.recv().await {
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
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
        for (key, arrived, score) in changes {
            let name = (key != UNKNOWN).then_some(key.clone());
            // Do not talk over the robot (or the AI) speaking; that greeting is skipped, not queued.
            let greet =
                arrived && !hub.status.lock().await.speaker_playing && faces.take_greeting(&key);
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
            if greet {
                let robot = robot.clone();
                tokio::spawn(async move {
                    if let Err(e) = robot
                        .say(&format!("Hi, {key}!"), Some("robot"), false)
                        .await
                    {
                        tracing::warn!("greeting failed: {e:#}");
                    }
                });
            }
        }
    }
}
