//! Things the agent wants to remember across sessions ("Justin likes jazz", "Kiwi went to the vet
//! on Monday"). Sessions start fresh every half hour with only a short recap; these facts are put
//! into every new session's instructions. Kept in `<data_dir>/memory.json`, readable only by the
//! user.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: u32,
    pub text: String,
    /// When it was remembered, as local "YYYY-MM-DD".
    pub date: String,
}

pub struct Memory {
    path: PathBuf,
    facts: Mutex<Vec<Fact>>,
}

/// A fact without what a model copied from the list it was shown ("12 | 2026-10-24 | ...").
pub fn clean_text(text: &str) -> &str {
    clean(text)
}

fn clean(text: &str) -> &str {
    let mut t = text.trim();
    loop {
        let Some((head, rest)) = t.split_once('|') else {
            return t;
        };
        let head = head.trim();
        let is_id = !head.is_empty() && head.chars().all(|c| c.is_ascii_digit());
        let is_date = chrono::NaiveDate::parse_from_str(head, "%Y-%m-%d").is_ok();
        if !(is_id || is_date) {
            return t;
        }
        t = rest.trim();
    }
}

/// Facts that no longer fit (the oldest, past `MAX_FACTS`): out of the instructions, but kept,
/// one JSON line each, for `crate::recall`.
fn archive_path(path: &Path) -> PathBuf {
    path.with_file_name("memory_archive.jsonl")
}

/// Longest fact kept.
const MAX_TEXT: usize = 300;
/// Most facts kept; the oldest go first.
const MAX_FACTS: usize = 100;

impl Memory {
    pub fn load(data_dir: &Path) -> Memory {
        let path = data_dir.join("memory.json");
        let mut facts: Vec<Fact> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        for f in &mut facts {
            f.text = clean(&f.text).to_string();
        }
        Memory {
            path,
            facts: Mutex::new(facts),
        }
    }

    pub fn remember(&self, text: &str) -> Result<Fact> {
        let text: String = text.trim().chars().take(MAX_TEXT).collect();
        if text.is_empty() {
            bail!("nothing to remember");
        }
        let mut facts = self.facts.lock().unwrap();
        if let Some(f) = facts.iter().find(|f| f.text.eq_ignore_ascii_case(&text)) {
            return Ok(f.clone());
        }
        let fact = Fact {
            id: facts.iter().map(|f| f.id).max().unwrap_or(0) + 1,
            text,
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        };
        facts.push(fact.clone());
        let excess = facts.len().saturating_sub(MAX_FACTS);
        let old: Vec<Fact> = facts.drain(..excess).collect();
        self.save(&facts)?;
        self.archive(&old);
        Ok(fact)
    }

    /// Replace the facts `ids` (two or more, all still there) with one, `text`: the same thing said
    /// several ways, or an older fact a newer one updates. Returns the new fact.
    pub fn merge(&self, ids: &[u32], text: &str) -> Result<Option<Fact>> {
        let text: String = clean(text).chars().take(MAX_TEXT).collect();
        let mut facts = self.facts.lock().unwrap();
        let merged: Vec<&Fact> = facts.iter().filter(|f| ids.contains(&f.id)).collect();
        if text.is_empty() || ids.len() < 2 || merged.len() != ids.len() {
            return Ok(None);
        }
        let fact = Fact {
            id: facts.iter().map(|f| f.id).max().unwrap_or(0) + 1,
            text,
            date: merged
                .iter()
                .map(|f| f.date.clone())
                .max()
                .unwrap_or_default(),
        };
        facts.retain(|f| !ids.contains(&f.id));
        facts.push(fact.clone());
        self.save(&facts)?;
        Ok(Some(fact))
    }

    /// Move the facts `ids` out of the instructions into the archive (still found by recall):
    /// passing things that are not worth knowing every day. Returns how many.
    pub fn retire(&self, ids: &[u32]) -> Result<usize> {
        let mut facts = self.facts.lock().unwrap();
        let (old, keep): (Vec<Fact>, Vec<Fact>) =
            facts.drain(..).partition(|f| ids.contains(&f.id));
        *facts = keep;
        if !old.is_empty() {
            self.save(&facts)?;
            self.archive(&old);
        }
        Ok(old.len())
    }

    /// Facts that no longer fit in the instructions (see `remember`), oldest first.
    pub fn archived(&self) -> Vec<Fact> {
        std::fs::read_to_string(archive_path(&self.path))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn archive(&self, old: &[Fact]) {
        if old.is_empty() {
            return;
        }
        let mut all = self.archived();
        all.extend_from_slice(old);
        let lines: String = all
            .iter()
            .filter_map(|f| serde_json::to_string(f).ok())
            .map(|l| l + "\n")
            .collect();
        if let Err(e) = crate::config::write_private(&archive_path(&self.path), lines.as_bytes()) {
            tracing::warn!("memory archive: {e:#}");
        }
    }

    /// Forget fact `id`; returns whether there was one.
    pub fn forget(&self, id: u32) -> Result<bool> {
        let mut facts = self.facts.lock().unwrap();
        let before = facts.len();
        facts.retain(|f| f.id != id);
        if facts.len() == before {
            return Ok(false);
        }
        self.save(&facts)?;
        Ok(true)
    }

    pub fn facts(&self) -> Vec<Fact> {
        self.facts.lock().unwrap().clone()
    }

    /// The facts as lines for the agent's instructions, empty if there are none.
    pub fn for_instructions(&self) -> String {
        self.facts()
            .iter()
            .map(|f| format!("- [{}] {} ({})\n", f.id, f.text, f.date))
            .collect()
    }

    fn save(&self, facts: &[Fact]) -> Result<()> {
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
        std::io::Write::write_all(&mut f, &serde_json::to_vec_pretty(facts)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_across_restarts_without_duplicates() {
        let dir = std::env::temp_dir().join(format!(
            "bbplane-memory-{}-{}",
            std::process::id(),
            crate::hub::now_ms()
        ));
        let m = Memory::load(&dir);
        let jazz = m.remember("Justin likes jazz").unwrap();
        assert_eq!(m.remember("justin likes JAZZ").unwrap().id, jazz.id);
        m.remember("Kiwi is scared of the vacuum").unwrap();
        assert!(m.remember("  ").is_err());

        let m = Memory::load(&dir);
        assert_eq!(m.facts().len(), 2);
        assert!(m.for_instructions().contains("Justin likes jazz"));
        assert!(m.forget(jazz.id).unwrap());
        assert!(!m.forget(jazz.id).unwrap());
        assert_eq!(Memory::load(&dir).facts().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("memory.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn keeps_the_newest_facts() {
        let dir = std::env::temp_dir().join(format!(
            "bbplane-memory-cap-{}-{}",
            std::process::id(),
            crate::hub::now_ms()
        ));
        let m = Memory::load(&dir);
        for i in 0..MAX_FACTS + 5 {
            m.remember(&format!("fact {i}")).unwrap();
        }
        let facts = m.facts();
        assert_eq!(facts.len(), MAX_FACTS);
        assert_eq!(facts[0].text, "fact 5");
        let old: Vec<String> = m.archived().into_iter().map(|f| f.text).collect();
        assert_eq!(
            old,
            ["fact 0", "fact 1", "fact 2", "fact 3", "fact 4"],
            "kept for recall"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn list_prefixes_are_not_part_of_a_fact() {
        assert_eq!(
            clean("2026-10-24 | The window is bright"),
            "The window is bright"
        );
        assert_eq!(clean("12 | 2026-10-24 | Kiwi naps"), "Kiwi naps");
        assert_eq!(
            clean("Tea | coffee: Justin likes both"),
            "Tea | coffee: Justin likes both"
        );
    }

    #[test]
    fn merges_only_whole_groups() {
        let dir = std::env::temp_dir().join(format!(
            "bbplane-memory-merge-{}-{}",
            std::process::id(),
            crate::hub::now_ms()
        ));
        let m = Memory::load(&dir);
        let a = m.remember("Kiwi likes the sunny spot").unwrap();
        let b = m.remember("Kiwi naps in the sun by the window").unwrap();
        let c = m.remember("Justin likes jazz").unwrap();
        assert!(m.merge(&[a.id], "one is not a group").unwrap().is_none());
        assert!(m.merge(&[a.id, 99], "a missing one").unwrap().is_none());
        let k = m
            .merge(&[a.id, b.id], "Kiwi naps in the sunny spot by the window")
            .unwrap()
            .unwrap();
        let texts: Vec<String> = Memory::load(&dir)
            .facts()
            .into_iter()
            .map(|f| f.text)
            .collect();
        assert_eq!(texts, [c.text.clone(), k.text]);
        // Retired: out of the instructions, still in the archive.
        assert_eq!(m.retire(&[c.id, 999]).unwrap(), 1);
        let m = Memory::load(&dir);
        assert_eq!(m.facts().len(), 1);
        assert!(m.archived().iter().any(|f| f.text == c.text));
        std::fs::remove_dir_all(dir).ok();
    }
}
