//! The handheld controller's home screen in AI mode (see `ToRobot::HomeInfo`): the clock, the
//! next reminder, the latest thing the robot noticed and the last thing it said. Sent when any of
//! it changes and every few minutes anyway (the robot keeps its own clock right with it).

use std::time::{Duration, Instant};

use bbproto::ctrl::ToRobot;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

use crate::control::Robot;
use crate::hub::Event;

/// Look for news this often.
const CHECK: Duration = Duration::from_secs(2);
/// Send it again this often even when nothing changed (the clock).
const REFRESH: Duration = Duration::from_secs(300);
/// Sightings older than this are not news.
const SEEN_FOR: Duration = Duration::from_secs(12 * 3600);
/// The robot keeps this much of each (it shows ASCII only).
const MAX_NEXT: usize = 48;
const MAX_SEEN: usize = 32;
const MAX_SAID: usize = 100;

#[derive(Debug, Clone, Default, PartialEq)]
struct News {
    next: String,
    seen: String,
    /// When it was seen, in Unix milliseconds.
    seen_ms: u64,
    said: String,
}

/// Keep the home screen up to date while the robot is in AI mode. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let mut sent: Option<(News, Instant)> = None;
    let mut tick = tokio::time::interval(CHECK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        // Sent again whenever the robot (re)enters AI mode: it forgets the news when it leaves.
        if !robot.hub.is_connected().await || robot.mode().await.as_deref() != Some("remote") {
            sent = None;
            continue;
        }
        let news = news(&robot).await;
        if sent
            .as_ref()
            .is_some_and(|(n, at)| *n == news && at.elapsed() < REFRESH)
        {
            continue;
        }
        match robot.hub.send(message(&news, Local::now())).await {
            Ok(()) => sent = Some((news, Instant::now())),
            Err(e) => tracing::debug!("home info: {e:#}"),
        }
    }
}

async fn news(robot: &Robot) -> News {
    let now_ms = Local::now().timestamp_millis();
    let next = robot
        .reminders
        .list()
        .into_iter()
        .filter(|r| r.due_ms >= now_ms - 60_000)
        .min_by_key(|r| r.due_ms)
        .map(|r| {
            let due = Local
                .timestamp_millis_opt(r.due_ms)
                .single()
                .unwrap_or_else(Local::now);
            format!("{} {}", when(due, Local::now()), r.text)
        })
        .unwrap_or_default();
    let seen = robot.hub.last_event(|e| sighting(e).is_some()).await;
    let (seen, seen_ms) = seen
        .filter(|e| (now_ms as u64).saturating_sub(e.ts) < SEEN_FOR.as_millis() as u64)
        .and_then(|e| sighting(&e).map(|s| (s, e.ts)))
        .unwrap_or_default();
    let said = robot
        .hub
        .last_event(|e| e.kind == "said")
        .await
        .and_then(|e| e.data["text"].as_str().map(str::to_string))
        .unwrap_or_default();
    News {
        next: ascii(&next, MAX_NEXT),
        seen: ascii(&seen, MAX_SEEN),
        seen_ms,
        said: ascii(&said, MAX_SAID),
    }
}

fn message(n: &News, now: DateTime<Local>) -> ToRobot {
    let seen_secs = if n.seen.is_empty() {
        0
    } else {
        (now.timestamp_millis() as u64).saturating_sub(n.seen_ms) / 1000
    };
    ToRobot::HomeInfo {
        clock: [
            now.year() as u16,
            now.month() as u16,
            now.day() as u16,
            now.hour() as u16,
            now.minute() as u16,
            now.second() as u16,
        ],
        next: n.next.clone(),
        seen: n.seen.clone(),
        seen_secs: seen_secs.min(u32::MAX as u64) as u32,
        said: n.said.clone(),
    }
}

/// When a reminder is due, short: "15:00" today, "Tue 15:00" this week, "Oct 30" later.
fn when(due: DateTime<Local>, now: DateTime<Local>) -> String {
    let days = (due.date_naive() - now.date_naive()).num_days();
    if days <= 0 {
        due.format("%H:%M").to_string()
    } else if days < 7 {
        due.format("%a %H:%M").to_string()
    } else {
        due.format("%b %-d").to_string()
    }
}

/// A line for something the robot saw or heard, if `e` is one.
fn sighting(e: &Event) -> Option<String> {
    let d = &e.data;
    match e.kind.as_str() {
        "face" if d["event"] == "arrived" => Some(match d["name"].as_str() {
            Some(name) => format!("Saw {name}"),
            None => "Saw someone".into(),
        }),
        "object" if d["event"] == "noticed" => d["kind"].as_str().map(|k| {
            let article = if k.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            format!("Spotted {article} {k}")
        }),
        "sound" => d["heard"].as_str().map(|h| format!("Heard: {h}")),
        _ => None,
    }
}

/// `s` in plain ASCII (the controller's font has nothing else), at most `max` bytes.
fn ascii(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    for c in s.chars() {
        let folded: &str = match c {
            '\u{2018}' | '\u{2019}' | '\u{2032}' => "'",
            '\u{201C}' | '\u{201D}' | '\u{2033}' => "\"",
            '\u{2013}' | '\u{2014}' | '\u{2212}' => "-",
            '\u{2026}' => "...",
            '\n' | '\r' | '\t' | '\u{00A0}' => " ",
            c if c.is_ascii() && !c.is_ascii_control() => {
                if out.len() < max {
                    out.push(c);
                }
                continue;
            }
            _ => "",
        };
        if out.len() + folded.len() <= max {
            out.push_str(folded);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ascii_folds_and_trims() {
        assert_eq!(
            ascii("It\u{2019}s \u{201C}fine\u{201D}\u{2026} \u{1F600}", 100),
            "It's \"fine\"..."
        );
        assert_eq!(ascii("abcdef", 4), "abcd");
        assert_eq!(ascii("line\nbreak", 100), "line break");
    }

    #[test]
    fn reminder_times_get_shorter_with_distance() {
        let now = Local.with_ymd_and_hms(2026, 10, 20, 9, 0, 0).unwrap();
        let today = Local.with_ymd_and_hms(2026, 10, 20, 15, 0, 0).unwrap();
        let thursday = Local.with_ymd_and_hms(2026, 10, 22, 15, 0, 0).unwrap();
        let later = Local.with_ymd_and_hms(2026, 11, 30, 15, 0, 0).unwrap();
        assert_eq!(when(today, now), "15:00");
        assert_eq!(when(thursday, now), "Thu 15:00");
        assert_eq!(when(later, now), "Nov 30");
    }

    #[test]
    fn sightings_read_naturally() {
        let ev = |kind: &str, data| Event {
            seq: 1,
            ts: 0,
            kind: kind.into(),
            data,
        };
        let face = ev("face", json!({ "event": "arrived", "name": "Justin" }));
        assert_eq!(sighting(&face).as_deref(), Some("Saw Justin"));
        let stranger = ev("face", json!({ "event": "arrived", "name": null }));
        assert_eq!(sighting(&stranger).as_deref(), Some("Saw someone"));
        let left = ev("face", json!({ "event": "left", "name": "Justin" }));
        assert_eq!(sighting(&left), None);
        let dog = ev("object", json!({ "event": "noticed", "kind": "dog" }));
        assert_eq!(sighting(&dog).as_deref(), Some("Spotted a dog"));
        let umbrella = ev("object", json!({ "event": "noticed", "kind": "umbrella" }));
        assert_eq!(sighting(&umbrella).as_deref(), Some("Spotted an umbrella"));
        let bark = ev("sound", json!({ "heard": "Bark", "score": 0.7 }));
        assert_eq!(sighting(&bark).as_deref(), Some("Heard: Bark"));
    }
}
