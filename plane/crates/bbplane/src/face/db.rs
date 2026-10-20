//! People enrolled for recognition: a name and a few face embeddings each, kept in one JSON
//! file. No images are stored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::engine::{EMBEDDING_LEN, similarity_score};

/// Samples kept per person; enrolling again adds to them, oldest dropped first.
pub const MAX_SAMPLES: usize = 30;
const MAX_NAME: usize = 40;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FaceDb {
    pub people: Vec<Person>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    pub samples: Vec<Vec<f32>>,
}

/// Best match for an embedding.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub name: String,
    pub score: f32,
}

impl FaceDb {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("faces.json")
    }

    /// Load from `path`; a missing file is an empty database.
    pub fn load(path: &Path) -> Result<FaceDb> {
        match std::fs::read(path) {
            Ok(b) => {
                let mut db: FaceDb = serde_json::from_slice(&b)
                    .with_context(|| format!("parsing {}", path.display()))?;
                for p in &mut db.people {
                    p.samples.retain(|s| s.len() == EMBEDDING_LEN);
                }
                db.people.retain(|p| !p.samples.is_empty());
                Ok(db)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FaceDb::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically, readable only by this user (face embeddings are biometric data).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec(self)?)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn names(&self) -> Vec<String> {
        self.people.iter().map(|p| p.name.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.people.is_empty()
    }

    /// Add samples for `name` (case-insensitive match on an existing person).
    pub fn enroll(&mut self, name: &str, samples: Vec<Vec<f32>>) -> Result<()> {
        let name = clean_name(name)?;
        let i = match self.find(&name) {
            Some(i) => i,
            None => {
                self.people.push(Person {
                    name,
                    samples: Vec::new(),
                });
                self.people.len() - 1
            }
        };
        let p = &mut self.people[i];
        p.samples.extend(samples);
        let excess = p.samples.len().saturating_sub(MAX_SAMPLES);
        p.samples.drain(..excess);
        Ok(())
    }

    /// Remove `name`; returns whether it was enrolled.
    pub fn forget(&mut self, name: &str) -> bool {
        match self.find(name) {
            Some(i) => {
                self.people.remove(i);
                true
            }
            None => false,
        }
    }

    /// The enrolled person most similar to `embedding`, if anyone reaches `threshold`.
    pub fn best_match(&self, embedding: &[f32], threshold: f32) -> Option<Match> {
        self.people
            .iter()
            .filter_map(|p| {
                let score = p
                    .samples
                    .iter()
                    .map(|s| similarity_score(s, embedding))
                    .fold(f32::MIN, f32::max);
                (score >= threshold).then(|| Match {
                    name: p.name.clone(),
                    score,
                })
            })
            .max_by(|a, b| a.score.total_cmp(&b.score))
    }

    fn find(&self, name: &str) -> Option<usize> {
        let name = normalize_name(name);
        self.people
            .iter()
            .position(|p| p.name.eq_ignore_ascii_case(&name))
    }
}

/// Collapse runs of whitespace; this is the form names are stored and compared in.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `name` normalized, or why it is not usable.
pub fn clean_name(name: &str) -> Result<String> {
    let name = normalize_name(name);
    if name.is_empty() {
        bail!("give the person a name");
    }
    if name.chars().count() > MAX_NAME {
        bail!("names are limited to {MAX_NAME} characters");
    }
    if name.chars().any(char::is_control) {
        bail!("the name contains control characters");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_LEN];
        v[i] = 1.0;
        v
    }

    #[test]
    fn enroll_match_forget() {
        let mut db = FaceDb::default();
        db.enroll("  Justin  ", vec![unit(0)]).unwrap();
        db.enroll("Sam  Lee", vec![unit(1)]).unwrap();
        assert!(db.forget(" sam   lee "));
        db.enroll("Sam", vec![unit(1)]).unwrap();
        db.enroll("justin", vec![unit(2)]).unwrap();
        assert_eq!(db.names(), ["Justin", "Sam"]);
        assert_eq!(db.people[0].samples.len(), 2);

        let m = db.best_match(&unit(2), 0.5).unwrap();
        assert_eq!((m.name.as_str(), m.score), ("Justin", 1.0));
        assert_eq!(db.best_match(&unit(1), 0.5).unwrap().name, "Sam");
        assert_eq!(db.best_match(&unit(3), 0.5), None);

        assert!(db.forget("SAM"));
        assert!(!db.forget("Sam"));
        assert_eq!(db.names(), ["Justin"]);
        assert!(db.enroll(" ", vec![unit(0)]).is_err());
    }

    #[test]
    fn keeps_newest_samples() {
        let mut db = FaceDb::default();
        db.enroll("A", (0..MAX_SAMPLES).map(|_| unit(0)).collect())
            .unwrap();
        db.enroll("A", vec![unit(1)]).unwrap();
        let s = &db.people[0].samples;
        assert_eq!(s.len(), MAX_SAMPLES);
        assert_eq!(s.last().unwrap(), &unit(1));
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("bbplane-facedb-{}", std::process::id()));
        let path = FaceDb::path(&dir);
        let mut db = FaceDb::default();
        db.enroll("A", vec![unit(5)]).unwrap();
        db.save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let back = FaceDb::load(&path).unwrap();
        assert_eq!(back.names(), ["A"]);
        assert_eq!(back.people[0].samples[0], unit(5));
        assert!(FaceDb::load(&dir.join("missing.json")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
