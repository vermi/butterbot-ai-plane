//! The robot's diary. A day's moments are logged as they happen (who came by, what it noticed
//! and heard, its moods, what it did and said); in the evening the agent gets a `diary` nudge with
//! a digest of them and writes the day up in its own words with `write_diary`. If it was off then,
//! it is asked the next morning. One Markdown file a day in diary/, with the day's log beside it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control::Robot;

/// Longest diary entry kept.
const MAX_ENTRY: usize = 4000;
/// A missed evening is caught up until this hour the next day.
const CATCH_UP_UNTIL: u32 = 12;
const MAX_DIGEST: usize = 2500;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Moment {
    /// Unix milliseconds.
    ts: i64,
    kind: String,
    text: String,
}

pub struct Diary {
    dir: PathBuf,
    /// The last day it was asked to write about (once a day is enough).
    asked: Mutex<Option<NaiveDate>>,
}

impl Diary {
    pub fn load(data_dir: &Path) -> Diary {
        let dir = data_dir.join("diary");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("diary folder: {e}");
        }
        Diary {
            dir,
            asked: Mutex::new(None),
        }
    }

    fn log_path(&self, day: NaiveDate) -> PathBuf {
        self.dir.join(format!("{day}.log.jsonl"))
    }

    fn entry_path(&self, day: NaiveDate) -> PathBuf {
        self.dir.join(format!("{day}.md"))
    }

    /// Note a moment of today.
    pub fn note(&self, kind: &str, text: &str) {
        let now = Local::now();
        let m = Moment {
            ts: now.timestamp_millis(),
            kind: kind.into(),
            text: text.chars().take(200).collect(),
        };
        let r = (|| -> Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.log_path(now.date_naive()))?;
            writeln!(f, "{}", serde_json::to_string(&m)?)?;
            Ok(())
        })();
        if let Err(e) = r {
            tracing::debug!("diary log: {e:#}");
        }
    }

    fn moments(&self, day: NaiveDate) -> Vec<Moment> {
        std::fs::read_to_string(self.log_path(day))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// What happened on `day`, in brief, for writing the entry.
    pub fn digest(&self, day: NaiveDate) -> String {
        let moments = self.moments(day);
        if moments.is_empty() {
            return "Nothing was noted that day.".into();
        }
        let time = |ts: i64| crate::reminders::local(ts).format("%H:%M").to_string();
        let mut counts: BTreeMap<(&str, &str), u32> = BTreeMap::new();
        let mut moods: Vec<String> = Vec::new();
        let mut talk: Vec<String> = Vec::new();
        for m in &moments {
            match m.kind.as_str() {
                "saw" | "spotted" | "heard" | "did" | "call" => {
                    *counts
                        .entry((m.kind.as_str(), m.text.as_str()))
                        .or_default() += 1;
                }
                "mood" if moods.last() != Some(&m.text) => {
                    moods.push(format!("{} {}", time(m.ts), m.text));
                }
                "said" => talk.push(format!("{} you: {}", time(m.ts), m.text)),
                "told" => talk.push(format!("{} to you: {}", time(m.ts), m.text)),
                _ => {}
            }
        }
        let mut out = format!(
            "From {} to {}.\n",
            time(moments[0].ts),
            time(moments[moments.len() - 1].ts)
        );
        for (kind, label) in [
            ("saw", "People"),
            ("spotted", "Noticed"),
            ("heard", "Heard"),
            ("did", "Did"),
            ("call", "Calls"),
        ] {
            let items: Vec<String> = counts
                .iter()
                .filter(|((k, _), _)| *k == kind)
                .map(|((_, t), n)| {
                    if *n > 1 {
                        format!("{t} ({n}x)")
                    } else {
                        t.to_string()
                    }
                })
                .collect();
            if !items.is_empty() {
                out.push_str(&format!("{label}: {}.\n", items.join(", ")));
            }
        }
        if !moods.is_empty() {
            out.push_str(&format!("Moods: {}.\n", moods.join("; ")));
        }
        if !talk.is_empty() {
            // A sample through the day
            let step = talk.len().div_ceil(14).max(1);
            out.push_str(&format!("Conversation ({} lines), a sample:\n", talk.len()));
            for line in talk.iter().step_by(step) {
                out.push_str(line);
                out.push('\n');
            }
        }
        if out.len() > MAX_DIGEST {
            let mut cut = MAX_DIGEST;
            while !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
        }
        out
    }

    pub fn write(&self, day: NaiveDate, text: &str) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            bail!("the entry is empty");
        }
        let text: String = text.chars().take(MAX_ENTRY).collect();
        std::fs::write(self.entry_path(day), text).context("saving the diary entry")
    }

    pub fn read(&self, day: NaiveDate) -> Option<String> {
        std::fs::read_to_string(self.entry_path(day)).ok()
    }

    /// Days with an entry, newest first.
    pub fn days(&self) -> Vec<NaiveDate> {
        let mut days: Vec<NaiveDate> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                let day = name.strip_suffix(".md")?;
                NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()
            })
            .collect();
        days.sort_unstable_by(|a, b| b.cmp(a));
        days
    }

    /// The day to write about now, if any: today once it is past `at`, or yesterday until noon
    /// if that was missed; only days with something in them, once each.
    pub fn due(&self, now: DateTime<Local>, at: NaiveTime) -> Option<NaiveDate> {
        let today = now.date_naive();
        let day = if now.time() >= at {
            today
        } else if now.hour() < CATCH_UP_UNTIL {
            today.pred_opt()?
        } else {
            return None;
        };
        if *self.asked.lock().unwrap() == Some(day)
            || self.entry_path(day).exists()
            || !self.log_path(day).exists()
        {
            return None;
        }
        Some(day)
    }

    pub fn asked(&self, day: NaiveDate) {
        *self.asked.lock().unwrap() = Some(day);
    }
}

/// Note the day's moments as they happen. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let mut cursor = robot.hub.last_event_seq().await;
    let mut last_mood = String::new();
    let mut last_doing = String::new();
    loop {
        let events = robot
            .hub
            .events_since(cursor, Duration::from_secs(30))
            .await;
        for e in &events {
            cursor = cursor.max(e.seq);
            if let Some((kind, text)) = moment(&e.kind, &e.data) {
                robot.diary.note(kind, &text);
            }
        }
        let mood = robot.mood.now();
        let m = if mood.why.is_empty() {
            mood.emotion.name().to_string()
        } else {
            format!("{} ({})", mood.emotion.name(), mood.why)
        };
        if m != last_mood {
            robot.diary.note("mood", &m);
            last_mood = m;
        }
        let doing = robot.doing().unwrap_or_default();
        if !doing.is_empty() && doing != last_doing {
            robot.diary.note("did", doing.trim_end_matches('!'));
        }
        last_doing = doing;
    }
}

/// A logged event worth remembering, as (kind, text).
fn moment(kind: &str, d: &Value) -> Option<(&'static str, String)> {
    let s = |k: &str| d[k].as_str().map(str::to_string);
    match kind {
        "face" if d["event"] == "arrived" => {
            Some(("saw", s("name").unwrap_or_else(|| "someone".into())))
        }
        "object" if d["event"] == "noticed" => Some(("spotted", s("kind")?)),
        "sound" => Some(("heard", s("heard")?)),
        "said" => Some(("said", s("text")?)),
        "heard" => Some((
            "told",
            match s("speaker") {
                Some(who) => format!("{who}: {}", s("text")?),
                None => s("text")?,
            },
        )),
        "phone_call" => Some(("call", s("from").unwrap_or_else(|| "someone".into()))),
        "appletv" => Some((
            "heard",
            match (s("title"), s("artist")) {
                (Some(t), Some(a)) if !a.is_empty() => format!("{t} by {a} (Apple TV)"),
                (Some(t), _) => format!("{t} (Apple TV)"),
                _ => return None,
            },
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_is_noted_digested_and_written_once() {
        let dir = std::env::temp_dir().join(format!("bbdiary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = Diary::load(&dir);
        let now = Local::now();
        let today = now.date_naive();
        assert_eq!(d.due(now, NaiveTime::MIN), None, "nothing happened");
        d.note("saw", "Justin");
        d.note("saw", "Justin");
        d.note("spotted", "dog");
        d.note("mood", "happy (Justin is here)");
        d.note("said", "Hi Justin!");
        let digest = d.digest(today);
        assert!(digest.contains("Justin (2x)"), "{digest}");
        assert!(digest.contains("dog"), "{digest}");
        assert!(digest.contains("happy"), "{digest}");
        assert_eq!(d.due(now, NaiveTime::MIN), Some(today));
        d.asked(today);
        assert_eq!(d.due(now, NaiveTime::MIN), None, "asked once");
        d.write(today, "A good day. Kiwi visited.").unwrap();
        assert_eq!(d.days(), vec![today]);
        assert!(d.read(today).unwrap().contains("Kiwi"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
