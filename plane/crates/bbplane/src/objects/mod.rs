//! Object detection in AI mode, on the laptop.
//!
//! While the robot streams video, about one frame a second goes through YOLOX-S. What is in view
//! shows in `status`, `who_is_here` and the live view. When one of the `notice` kinds (a cat, a
//! dog, a bird) comes into view, the agent gets a nudge, so it can react on its own. A person in
//! view counts as company even when the low camera sees no face.

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

use crate::config::ObjectsConfig;
use crate::control::Robot;
use crate::hub::Hub;
use engine::Engine;

/// Analyse at most one frame per this interval.
const PERIOD: Duration = Duration::from_secs(1);
/// Consecutive analysed frames a kind of thing must appear in before it counts as arrived.
const ARRIVE_HITS: u32 = 2;
/// A kind of thing not seen for this long has gone.
const LEAVE_AFTER: Duration = Duration::from_secs(10);
/// `in_view` older than this is stale.
const VIEW_FRESH: Duration = Duration::from_secs(3);
/// After a failed model load (usually the download), wait this long before trying again.
const LOAD_RETRY: Duration = Duration::from_secs(60);
/// At the robot's resolution the detector mixes these up (a dog lying down scores as a cat), so
/// they are tracked as one kind: one animal coming into view is noticed once.
const ANIMALS: [&str; 10] = [
    "bird", "cat", "dog", "horse", "sheep", "cow", "elephant", "bear", "zebra", "giraffe",
];

/// Whether the detector's class is one of the animals it mixes up.
pub fn is_animal(class: &str) -> bool {
    ANIMALS.contains(&class)
}

/// The kind a class is tracked as.
fn kind(class: &'static str) -> &'static str {
    if ANIMALS.contains(&class) {
        "animal"
    } else {
        class
    }
}

/// An object in the latest analysed frame.
#[derive(Debug, Clone, Serialize)]
pub struct Seen {
    pub class: &'static str,
    pub score: f32,
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
    last_noticed: Option<Instant>,
}

#[derive(Default)]
struct View {
    seen: Vec<Seen>,
    at: Option<Instant>,
    tracks: HashMap<&'static str, Track>,
}

pub struct Objects {
    cfg: ObjectsConfig,
    /// Pets live here (`[life] pets`): animals are noticed less often.
    pets: bool,
    models_dir: PathBuf,
    engine: OnceCell<Arc<Engine>>,
    /// When and why loading the model last failed.
    load_failure: Mutex<Option<(Instant, String)>>,
    view: Mutex<View>,
}

impl Objects {
    pub fn new(hub: Arc<Hub>) -> Objects {
        let cfg = hub.config.objects.clone();
        for kind in &cfg.notice {
            if !engine::CLASSES.contains(&kind.as_str()) {
                tracing::warn!(
                    "[objects] notice: \"{kind}\" is not something the detector knows; see `bbplane config`"
                );
            }
        }
        Objects {
            cfg,
            pets: !hub.config.life.pets.is_empty(),
            models_dir: hub.config.data_dir().join("models"),
            engine: OnceCell::new(),
            load_failure: Mutex::new(None),
            view: Mutex::new(View::default()),
        }
    }

    /// Objects in the latest analysed frame (empty if that is stale).
    pub fn in_view(&self) -> Vec<Seen> {
        let v = self.view.lock().unwrap();
        match v.at {
            Some(at) if at.elapsed() < VIEW_FRESH => v.seen.clone(),
            _ => Vec::new(),
        }
    }

    /// What the detector saw in a frame analysed after `t` (None until there is one).
    pub fn seen_after(&self, t: Instant) -> Option<Vec<Seen>> {
        let v = self.view.lock().unwrap();
        v.at.filter(|at| *at > t).map(|_| v.seen.clone())
    }

    /// The kinds of things in view, most confident first, without repeats.
    pub fn kinds_in_view(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = Vec::new();
        for s in self.in_view() {
            if !kinds.contains(&s.class) {
                kinds.push(s.class);
            }
        }
        kinds
    }

    /// Someone is in front of the camera, face visible or not.
    pub fn person_in_view(&self) -> bool {
        self.in_view().iter().any(|s| s.class == "person")
    }

    pub fn status_json(&self) -> Value {
        json!({
            "enabled": self.cfg.enabled,
            "notice": self.cfg.notice,
            "in_view": self.in_view(),
        })
    }

    /// The model, loading (and on first use downloading, ~36 MB) it if needed.
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
                    .context("loading the object detection model")
            })
            .await
            .cloned();
        if let Err(e) = &r {
            *self.load_failure.lock().unwrap() = Some((Instant::now(), format!("{e:#}")));
        }
        r
    }

    /// Analyse one frame, update what is in view, and return what just came into view and is
    /// worth noticing (as the detector named it).
    async fn observe(&self, frame: &VideoFrame) -> Result<Vec<&'static str>> {
        let engine = self.engine().await?;
        let jpeg = frame.jpeg.clone();
        let min_score = self.cfg.threshold;
        let dets = tokio::task::spawn_blocking(move || {
            let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                .context("decoding camera frame")?
                .to_rgb8();
            engine.detect(&img, min_score)
        })
        .await
        .map_err(|e| anyhow!("object detection panicked: {e}"))??;
        let seen: Vec<Seen> = dets
            .into_iter()
            .map(|d| Seen {
                class: d.class,
                score: d.score,
                bbox: d.bbox,
                frame_width: frame.width,
                frame_height: frame.height,
            })
            .collect();
        let mut v = self.view.lock().unwrap();
        let noticed = update(
            &mut v.tracks,
            seen.iter().map(|s| kind(s.class)),
            Instant::now(),
            |k| {
                self.cfg
                    .notice
                    .iter()
                    .filter_map(|n| engine::CLASSES.iter().find(|c| *c == n))
                    .any(|c| kind(c) == k)
            },
            |k| {
                Duration::from_secs(if k == "animal" && self.pets {
                    self.cfg.pet_cooldown_secs
                } else {
                    self.cfg.notice_cooldown_secs
                })
            },
        )
        .into_iter()
        // `seen` is best first: name it after its most confident detection.
        .filter_map(|k| seen.iter().find(|s| kind(s.class) == k).map(|s| s.class))
        .collect();
        v.seen = seen;
        v.at = Some(Instant::now());
        Ok(noticed)
    }

    /// The video stopped (AI mode ended, link lost): nothing can be seen.
    fn sweep(&self) {
        let mut v = self.view.lock().unwrap();
        v.seen.clear();
        for t in v.tracks.values_mut() {
            t.hits = 0;
            t.present = false;
        }
    }
}

/// Track which kinds of things are present; returns those that just came into view, are worth
/// noticing and were not noticed within `cooldown`.
fn update(
    tracks: &mut HashMap<&'static str, Track>,
    seen: impl Iterator<Item = &'static str>,
    now: Instant,
    worth_noticing: impl Fn(&str) -> bool,
    cooldown: impl Fn(&str) -> Duration,
) -> Vec<&'static str> {
    let mut here: Vec<&'static str> = seen.collect();
    here.sort_unstable();
    here.dedup();
    let mut noticed = Vec::new();
    for kind in &here {
        let t = tracks.entry(kind).or_default();
        t.hits += 1;
        t.last_seen = Some(now);
        if !t.present && t.hits >= ARRIVE_HITS {
            t.present = true;
            if worth_noticing(kind) && t.last_noticed.is_none_or(|n| now - n >= cooldown(kind)) {
                t.last_noticed = Some(now);
                noticed.push(*kind);
            }
        }
    }
    for (kind, t) in tracks.iter_mut() {
        if here.contains(kind) {
            continue;
        }
        t.hits = 0;
        if t.present && t.last_seen.is_none_or(|s| now - s > LEAVE_AFTER) {
            t.present = false;
        }
    }
    noticed
}

/// Watch the live video while the robot streams it. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let objects = robot.objects.clone();
    let hub = robot.hub.clone();
    if !objects.cfg.enabled {
        return;
    }
    let mut rx = hub.video.subscribe();
    let mut last = Instant::now() - PERIOD;
    let mut last_error = String::new();
    loop {
        match tokio::time::timeout(LEAVE_AFTER, rx.recv()).await {
            Ok(Ok(_)) => {}
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => return,
            Err(_) => {
                objects.sweep();
                continue;
            }
        }
        if last.elapsed() < PERIOD {
            continue;
        }
        last = Instant::now();
        // Work on the newest frame, not whichever one woke us.
        let Some(frame) = hub.latest_frame().await else {
            continue;
        };
        let noticed = match objects.observe(&frame).await {
            Ok(n) => {
                last_error.clear();
                n
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg != last_error {
                    tracing::warn!("object detection: {msg}");
                    last_error = msg;
                }
                continue;
            }
        };
        for kind in noticed {
            tracing::info!("noticed a {kind}");
            robot.life.noticed(kind);
            hub.push_event("object", json!({ "event": "noticed", "kind": kind }))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_a_newcomer_once_per_cooldown() {
        let mut tracks = HashMap::new();
        let cats = |n: &str| n == "cat";
        let cooldown = Duration::from_secs(600);
        let t0 = Instant::now();
        let step = |tracks: &mut HashMap<_, _>, kinds: &[&'static str], at: Duration| {
            update(tracks, kinds.iter().copied(), t0 + at, cats, |_| cooldown)
        };
        // A cup is there all along: never noticed. The cat needs two frames in a row.
        assert!(step(&mut tracks, &["cup", "cat"], Duration::ZERO).is_empty());
        assert_eq!(
            step(&mut tracks, &["cup", "cat"], Duration::from_secs(1)),
            ["cat"]
        );
        assert!(step(&mut tracks, &["cup", "cat"], Duration::from_secs(2)).is_empty());
        // It wanders off and comes back a minute later: too soon to mention again.
        step(&mut tracks, &["cup"], Duration::from_secs(20));
        step(&mut tracks, &["cat"], Duration::from_secs(60));
        assert!(step(&mut tracks, &["cat"], Duration::from_secs(61)).is_empty());
        // Gone for a while, back after the cooldown: noticed again.
        step(&mut tracks, &[], Duration::from_secs(80));
        step(&mut tracks, &["cat"], Duration::from_secs(700));
        assert_eq!(
            step(&mut tracks, &["cat"], Duration::from_secs(701)),
            ["cat"]
        );
    }

    #[test]
    fn animals_are_one_kind() {
        assert_eq!(kind("dog"), "animal");
        assert_eq!(kind("cat"), "animal");
        assert_eq!(kind("cup"), "cup");
        // A dog the detector calls a cat in one frame and a dog in the next is still arriving.
        let mut tracks = HashMap::new();
        let t0 = Instant::now();
        let animal = |k: &str| k == "animal";
        let cooldown = Duration::from_secs(600);
        assert!(
            update(&mut tracks, [kind("cat")].into_iter(), t0, animal, |_| {
                cooldown
            })
            .is_empty()
        );
        assert_eq!(
            update(
                &mut tracks,
                [kind("dog")].into_iter(),
                t0 + Duration::from_secs(1),
                animal,
                |_| cooldown
            ),
            ["animal"]
        );
    }
}
