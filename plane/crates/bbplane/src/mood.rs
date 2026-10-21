//! How the robot feels: an emotional state that lasts.
//!
//! The agent owns it: it sets it with `feel` whenever something moves it, and gets it back with
//! every event, so a feeling carries from one moment (and session) to the next. What happens to the
//! robot moves it too (someone arriving, being shaken or poked, a long stretch of nothing, the
//! small hours), unless the agent feels something stronger. It fades back to content over an hour
//! or so and survives restarts. It sets how soon the robot's own ideas come (bored and curious
//! sooner, sleepy and sad later) and shows on the handheld controller's face. Focus mode still
//! wins: no nudges, whatever the mood.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Timelike;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A feeling halves in strength this often.
const HALF_LIFE_MINUTES: f64 = 40.0;
/// Weaker than this, it has faded: content again.
const FADED: f32 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Emotion {
    #[default]
    Content,
    Happy,
    Excited,
    Curious,
    Playful,
    Calm,
    Sleepy,
    Bored,
    Sad,
    Grumpy,
    Surprised,
    Worried,
}

impl Emotion {
    /// In wire order (`ToRobot::AiStatus`, the controller's face).
    pub const ALL: [Emotion; 12] = [
        Emotion::Content,
        Emotion::Happy,
        Emotion::Excited,
        Emotion::Curious,
        Emotion::Playful,
        Emotion::Calm,
        Emotion::Sleepy,
        Emotion::Bored,
        Emotion::Sad,
        Emotion::Grumpy,
        Emotion::Surprised,
        Emotion::Worried,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Emotion::Content => "content",
            Emotion::Happy => "happy",
            Emotion::Excited => "excited",
            Emotion::Curious => "curious",
            Emotion::Playful => "playful",
            Emotion::Calm => "calm",
            Emotion::Sleepy => "sleepy",
            Emotion::Bored => "bored",
            Emotion::Sad => "sad",
            Emotion::Grumpy => "grumpy",
            Emotion::Surprised => "surprised",
            Emotion::Worried => "worried",
        }
    }

    pub fn parse(s: &str) -> Option<Emotion> {
        Emotion::ALL
            .into_iter()
            .find(|e| e.name().eq_ignore_ascii_case(s.trim()))
    }

    pub fn wire(self) -> u8 {
        Emotion::ALL.iter().position(|e| *e == self).unwrap_or(0) as u8
    }

    /// Multiplier on the waits between its own ideas, at full strength.
    fn pace(self) -> f64 {
        match self {
            Emotion::Excited | Emotion::Bored => 0.5,
            Emotion::Curious | Emotion::Playful => 0.65,
            Emotion::Happy | Emotion::Surprised => 0.85,
            Emotion::Content | Emotion::Worried => 1.0,
            Emotion::Calm => 1.4,
            Emotion::Sad | Emotion::Grumpy => 1.6,
            Emotion::Sleepy => 2.5,
        }
    }
}

/// A feeling as it was set.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Felt {
    emotion: Emotion,
    intensity: f32,
    why: String,
    /// Unix milliseconds.
    since_ms: i64,
    /// The agent chose it (rather than something that happened).
    by_self: bool,
}

impl Default for Felt {
    fn default() -> Felt {
        Felt {
            emotion: Emotion::Content,
            intensity: 0.0,
            why: String::new(),
            since_ms: crate::hub::now_ms() as i64,
            by_self: false,
        }
    }
}

/// How it feels right now (faded, and with the time of day).
#[derive(Debug, Clone, PartialEq)]
pub struct Now {
    pub emotion: Emotion,
    /// 0..1.
    pub intensity: f32,
    pub why: String,
    pub minutes: u64,
    pub by_self: bool,
}

pub struct Mood {
    path: PathBuf,
    felt: Mutex<Felt>,
}

impl Mood {
    pub fn load(dir: &Path) -> Mood {
        let path = dir.join("mood.json");
        let felt = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Mood {
            path,
            felt: Mutex::new(felt),
        }
    }

    /// The agent's own feeling: always taken.
    pub fn feel(&self, emotion: Emotion, intensity: f32, why: &str) -> Now {
        self.set(emotion, intensity, why, true);
        self.now()
    }

    /// Something happened that would move it: taken unless it already feels something at least
    /// as strongly. Returns whether it was.
    pub fn appraise(&self, emotion: Emotion, intensity: f32, why: &str) -> bool {
        let now = self.now();
        if now.emotion != Emotion::Content && now.intensity >= intensity && now.emotion != emotion {
            return false;
        }
        self.set(
            emotion,
            intensity.max(if now.emotion == emotion {
                now.intensity
            } else {
                0.0
            }),
            why,
            false,
        );
        true
    }

    fn set(&self, emotion: Emotion, intensity: f32, why: &str, by_self: bool) {
        let felt = Felt {
            emotion,
            intensity: intensity.clamp(0.0, 1.0),
            why: why.chars().take(120).collect(),
            since_ms: crate::hub::now_ms() as i64,
            by_self,
        };
        tracing::info!(
            "mood: {} {:.1} ({}){}",
            emotion.name(),
            felt.intensity,
            felt.why,
            if by_self { ", its own" } else { "" }
        );
        if let Ok(b) = serde_json::to_vec_pretty(&felt)
            && let Err(e) = std::fs::write(&self.path, b)
        {
            tracing::warn!("saving the mood: {e}");
        }
        *self.felt.lock().unwrap() = felt;
    }

    pub fn now(&self) -> Now {
        let felt = self.felt.lock().unwrap().clone();
        now_from(
            &felt,
            crate::hub::now_ms() as i64,
            chrono::Local::now().hour(),
        )
    }

    /// For the agent's events.
    pub fn for_agent(&self) -> Value {
        let n = self.now();
        let mut v = json!({
            "feeling": n.emotion.name(),
            "intensity": (n.intensity * 10.0).round() / 10.0,
            "for_minutes": n.minutes,
        });
        if !n.why.is_empty() {
            v["why"] = n.why.into();
        }
        if n.by_self {
            v["your_choice"] = true.into();
        }
        v
    }

    /// Multiplier on the waits between the robot's own ideas.
    pub fn pace(&self) -> f64 {
        let n = self.now();
        1.0 + (n.emotion.pace() - 1.0) * n.intensity as f64
    }
}

fn now_from(felt: &Felt, now_ms: i64, hour: u32) -> Now {
    let minutes = ((now_ms - felt.since_ms).max(0) as f64) / 60_000.0;
    let intensity = felt.intensity * 0.5f32.powf((minutes / HALF_LIFE_MINUTES) as f32);
    if intensity >= FADED && felt.emotion != Emotion::Content {
        return Now {
            emotion: felt.emotion,
            intensity,
            why: felt.why.clone(),
            minutes: minutes as u64,
            by_self: felt.by_self,
        };
    }
    // Nothing in particular: the small hours make it sleepy.
    if !(7..23).contains(&hour) {
        return Now {
            emotion: Emotion::Sleepy,
            intensity: 0.4,
            why: "it is late".into(),
            minutes: 0,
            by_self: false,
        };
    }
    Now {
        emotion: Emotion::Content,
        intensity: 0.3,
        why: String::new(),
        minutes: minutes as u64,
        by_self: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn felt(emotion: Emotion, intensity: f32, minutes_ago: i64) -> Felt {
        Felt {
            emotion,
            intensity,
            why: "x".into(),
            since_ms: 1_000_000_000 - minutes_ago * 60_000,
            by_self: true,
        }
    }

    #[test]
    fn feelings_fade_back_to_content() {
        let now = 1_000_000_000;
        let fresh = now_from(&felt(Emotion::Bored, 0.8, 0), now, 12);
        assert_eq!(fresh.emotion, Emotion::Bored);
        assert!((fresh.intensity - 0.8).abs() < 1e-3);
        let later = now_from(&felt(Emotion::Bored, 0.8, 40), now, 12);
        assert!((later.intensity - 0.4).abs() < 1e-3, "{}", later.intensity);
        let gone = now_from(&felt(Emotion::Bored, 0.8, 200), now, 12);
        assert_eq!(gone.emotion, Emotion::Content);
        let night = now_from(&felt(Emotion::Bored, 0.8, 200), now, 2);
        assert_eq!(night.emotion, Emotion::Sleepy);
    }

    #[test]
    fn events_do_not_override_a_stronger_feeling() {
        let dir = std::env::temp_dir().join(format!("bbmood-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = Mood::load(&dir);
        m.feel(Emotion::Excited, 0.9, "a new idea");
        assert!(!m.appraise(Emotion::Bored, 0.4, "nothing happening"));
        assert_eq!(m.now().emotion, Emotion::Excited);
        assert!(m.appraise(Emotion::Grumpy, 0.95, "shaken hard"));
        assert_eq!(m.now().emotion, Emotion::Grumpy);
        // It is kept across restarts.
        assert_eq!(Mood::load(&dir).now().emotion, Emotion::Grumpy);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mood_sets_the_pace() {
        let dir = std::env::temp_dir().join(format!("bbmood-pace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = Mood::load(&dir);
        m.feel(Emotion::Sleepy, 1.0, "tired");
        assert!(m.pace() > 2.0);
        m.feel(Emotion::Bored, 1.0, "nothing to do");
        assert!(m.pace() < 0.6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wire_order_is_stable() {
        assert_eq!(Emotion::Content.wire(), 0);
        assert_eq!(Emotion::Worried.wire(), 11);
        assert_eq!(Emotion::parse("Curious"), Some(Emotion::Curious));
    }
}
