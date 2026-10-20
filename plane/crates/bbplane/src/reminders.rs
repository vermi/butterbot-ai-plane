//! Reminders the agent sets for people ("remind me in 20 minutes to take the pizza out"). They
//! are kept in `<data_dir>/reminders.json`, so they survive a restart, and come back to the agent
//! as a nudge when due, in focus mode too: someone asked for them. With no agent to pass one on,
//! the robot says it itself.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    pub id: u32,
    /// What to remind them of.
    pub text: String,
    /// When, in Unix milliseconds.
    pub due_ms: i64,
    /// When it was set, in Unix milliseconds.
    pub set_ms: i64,
}

impl Reminder {
    /// Local wall-clock time it is due, like "18:30".
    pub fn due_local(&self) -> String {
        local(self.due_ms).format("%H:%M").to_string()
    }

    pub fn set_local(&self) -> String {
        local(self.set_ms).format("%H:%M").to_string()
    }
}

fn local(ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Local::now)
}

pub struct Reminders {
    path: PathBuf,
    list: Mutex<Vec<Reminder>>,
}

/// Longest reminder text kept.
const MAX_TEXT: usize = 300;
/// Most reminders kept at once.
const MAX_REMINDERS: usize = 50;

impl Reminders {
    pub fn load(data_dir: &Path) -> Reminders {
        let path = data_dir.join("reminders.json");
        let list = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Reminders {
            path,
            list: Mutex::new(list),
        }
    }

    /// Set a reminder `in_minutes` from now, or at the next `at` ("HH:MM", local time).
    pub fn add(&self, text: &str, in_minutes: Option<f64>, at: Option<&str>) -> Result<Reminder> {
        let now = Local::now();
        let due = match (in_minutes, at) {
            (Some(m), _) if m.is_finite() && m > 0.0 && m <= 7.0 * 24.0 * 60.0 => {
                now + chrono::Duration::milliseconds((m * 60_000.0) as i64)
            }
            (_, Some(at)) => next_at(now, at)?,
            _ => bail!("give in_minutes (more than 0, at most a week) or at (\"HH:MM\")"),
        };
        let text: String = text.trim().chars().take(MAX_TEXT).collect();
        if text.is_empty() {
            bail!("what should the reminder say?");
        }
        let mut list = self.list.lock().unwrap();
        if list.len() >= MAX_REMINDERS {
            bail!("too many reminders; cancel some first");
        }
        let r = Reminder {
            id: list.iter().map(|r| r.id).max().unwrap_or(0) + 1,
            text,
            due_ms: due.timestamp_millis(),
            set_ms: now.timestamp_millis(),
        };
        list.push(r.clone());
        list.sort_by_key(|r| r.due_ms);
        self.save(&list)?;
        Ok(r)
    }

    pub fn list(&self) -> Vec<Reminder> {
        self.list.lock().unwrap().clone()
    }

    /// Cancel reminder `id`; returns whether there was one.
    pub fn cancel(&self, id: u32) -> Result<bool> {
        let mut list = self.list.lock().unwrap();
        let before = list.len();
        list.retain(|r| r.id != id);
        if list.len() == before {
            return Ok(false);
        }
        self.save(&list)?;
        Ok(true)
    }

    /// Take the earliest reminder due by `now_ms` (it is then gone).
    pub fn take_due(&self, now_ms: i64) -> Option<Reminder> {
        let mut list = self.list.lock().unwrap();
        let i = list.iter().position(|r| r.due_ms <= now_ms)?;
        let r = list.remove(i);
        if let Err(e) = self.save(&list) {
            tracing::warn!("saving reminders: {e:#}");
        }
        Some(r)
    }

    /// Whether a reminder has been due for at least `ms` (nobody has passed it on).
    pub fn overdue(&self, now_ms: i64, ms: i64) -> bool {
        self.list
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.due_ms + ms <= now_ms)
    }

    fn save(&self, list: &[Reminder]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec_pretty(list)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// The next time the clock reads `at` ("HH:MM", or "H:MM").
fn next_at(now: DateTime<Local>, at: &str) -> Result<DateTime<Local>> {
    let t = NaiveTime::parse_from_str(at.trim(), "%H:%M")
        .with_context(|| format!("\"{at}\" is not a time like 18:30"))?;
    let mut day = now.date_naive();
    for _ in 0..2 {
        if let Some(due) = Local.from_local_datetime(&day.and_time(t)).earliest()
            && due > now
        {
            return Ok(due);
        }
        day = day.succ_opt().context("date out of range")?;
    }
    bail!("could not work out when {at} is")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bbplane-reminders-{}-{}",
            std::process::id(),
            crate::hub::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reminders_come_due_in_order_and_survive_a_restart() {
        let dir = temp();
        let r = Reminders::load(&dir);
        let later = r.add("call mum", Some(30.0), None).unwrap();
        let soon = r.add("take the pizza out", Some(1.0), None).unwrap();
        assert!(r.add("", Some(1.0), None).is_err());
        assert!(r.add("nothing", None, None).is_err());
        assert!(r.add("never", Some(-5.0), None).is_err());

        let r = Reminders::load(&dir);
        assert_eq!(r.list().len(), 2);
        let now = Local::now().timestamp_millis();
        assert_eq!(r.take_due(now), None);
        assert_eq!(r.take_due(soon.due_ms).unwrap().text, "take the pizza out");
        assert!(r.overdue(later.due_ms + 60_000, 60_000));
        assert!(r.cancel(later.id).unwrap());
        assert!(r.list().is_empty());
        assert!(!r.cancel(later.id).unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn at_means_the_next_time_the_clock_says_so() {
        let now = Local.with_ymd_and_hms(2026, 10, 20, 18, 0, 0).unwrap();
        assert_eq!(
            next_at(now, "18:30").unwrap(),
            Local.with_ymd_and_hms(2026, 10, 20, 18, 30, 0).unwrap()
        );
        assert_eq!(
            next_at(now, "7:15").unwrap(),
            Local.with_ymd_and_hms(2026, 10, 21, 7, 15, 0).unwrap()
        );
        assert!(next_at(now, "half past six").is_err());
    }
}
