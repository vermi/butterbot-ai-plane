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

/// Longest fact kept.
const MAX_TEXT: usize = 300;
/// Most facts kept; the oldest go first.
const MAX_FACTS: usize = 100;

impl Memory {
    pub fn load(data_dir: &Path) -> Memory {
        let path = data_dir.join("memory.json");
        let facts = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
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
        facts.drain(..excess);
        self.save(&facts)?;
        Ok(fact)
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
        std::fs::remove_dir_all(dir).ok();
    }
}
