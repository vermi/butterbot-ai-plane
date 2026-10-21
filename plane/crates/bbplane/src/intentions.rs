//! The robot's own plans: things it means to do, at a time ("check on Kiwi after lunch") or some
//! day ("find out what that clicking noise was"). The agent keeps them with `intend`. A timed one
//! comes back as a `plan` nudge when it is due, unless it is in focus mode or quiet; the rest come
//! along with idle nudges, as its own ideas. Kept in intentions.json.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::reminders::{local, next_at};

/// Longest plan kept.
const MAX_TEXT: usize = 200;
/// Most plans kept at once.
const MAX_PLANS: usize = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intention {
    pub id: u32,
    pub text: String,
    /// When, in Unix milliseconds; none: some day.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<i64>,
    /// When it was made, in Unix milliseconds.
    pub set_ms: i64,
}

impl Intention {
    /// "15:00" or "some day".
    pub fn when(&self) -> String {
        self.due_ms
            .map(|ms| local(ms).format("%a %H:%M").to_string())
            .unwrap_or_else(|| "some day".into())
    }
}

pub struct Intentions {
    path: PathBuf,
    list: Mutex<Vec<Intention>>,
}

impl Intentions {
    pub fn load(data_dir: &Path) -> Intentions {
        let path = data_dir.join("intentions.json");
        let list = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Intentions {
            path,
            list: Mutex::new(list),
        }
    }

    /// A plan for `in_minutes` from now, the next `at` ("HH:MM"), or some day (neither).
    pub fn add(&self, text: &str, in_minutes: Option<f64>, at: Option<&str>) -> Result<Intention> {
        let now = Local::now();
        let due = match (in_minutes, at) {
            (Some(m), _) if m.is_finite() && m > 0.0 && m <= 7.0 * 24.0 * 60.0 => {
                Some(now + chrono::Duration::milliseconds((m * 60_000.0) as i64))
            }
            (Some(_), _) => bail!("in_minutes must be more than 0 and at most a week"),
            (_, Some(at)) => Some(next_at(now, at)?),
            _ => None,
        };
        let text: String = text.trim().chars().take(MAX_TEXT).collect();
        if text.is_empty() {
            bail!("what do you mean to do?");
        }
        let mut list = self.list.lock().unwrap();
        if list.len() >= MAX_PLANS {
            bail!("that is a lot of plans already; drop some with `plans`");
        }
        let i = Intention {
            id: list.iter().map(|i| i.id).max().unwrap_or(0) + 1,
            text,
            due_ms: due.map(|d| d.timestamp_millis()),
            set_ms: now.timestamp_millis(),
        };
        list.push(i.clone());
        list.sort_by_key(|i| i.due_ms.unwrap_or(i64::MAX));
        self.save(&list)?;
        Ok(i)
    }

    pub fn list(&self) -> Vec<Intention> {
        self.list.lock().unwrap().clone()
    }

    /// Done with plan `id` (did it, or dropped it); returns whether there was one.
    pub fn done(&self, id: u32) -> Result<bool> {
        let mut list = self.list.lock().unwrap();
        let before = list.len();
        list.retain(|i| i.id != id);
        let gone = list.len() != before;
        if gone {
            self.save(&list)?;
        }
        Ok(gone)
    }

    /// A timed plan that is due, taken off the list (it can be made again if put off).
    pub fn take_due(&self, now_ms: i64) -> Option<Intention> {
        let mut list = self.list.lock().unwrap();
        let i = list
            .iter()
            .position(|i| i.due_ms.is_some_and(|d| d <= now_ms))?;
        let it = list.remove(i);
        if let Err(e) = self.save(&list) {
            tracing::warn!("saving plans: {e:#}");
        }
        Some(it)
    }

    /// Plans without a time, oldest first.
    pub fn some_day(&self, n: usize) -> Vec<Intention> {
        self.list
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.due_ms.is_none())
            .take(n)
            .cloned()
            .collect()
    }

    /// The next plan: the soonest timed one, else the oldest some-day one.
    pub fn next(&self) -> Option<Intention> {
        self.list.lock().unwrap().first().cloned()
    }

    fn save(&self, list: &[Intention]) -> Result<()> {
        std::fs::write(&self.path, serde_json::to_vec_pretty(list)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_come_due_and_some_day_ones_wait() {
        let dir = std::env::temp_dir().join(format!("bbplans-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = Intentions::load(&dir);
        let later = p.add("find out what clicks", None, None).unwrap();
        let soon = p.add("check on Kiwi", Some(1.0), None).unwrap();
        assert_eq!(p.next().unwrap().id, soon.id, "timed plans first");
        assert!(p.take_due(Local::now().timestamp_millis()).is_none());
        let due = p
            .take_due(Local::now().timestamp_millis() + 120_000)
            .unwrap();
        assert_eq!(due.id, soon.id);
        assert_eq!(p.some_day(5).len(), 1);
        assert!(p.done(later.id).unwrap());
        assert!(Intentions::load(&dir).list().is_empty(), "kept on disk");
        std::fs::remove_dir_all(&dir).ok();
    }
}
