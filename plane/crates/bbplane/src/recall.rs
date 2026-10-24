//! Recall: everything it knows, searchable by meaning. Its memories (including those that no
//! longer fit in its instructions), its diary, and what each day held (who came by, what it
//! noticed, its conversations, its dreams) are embedded with OpenAI's embedding model and kept in
//! `<data_dir>/recall.json`. What someone says brings related things to mind (`brings_to_mind`),
//! and the `recall` tool searches on request. The index is brought up to date in the background
//! (`run`) and while it naps (`crate::brain::sleep`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::brain::text::{API, client};
use crate::config::BrainConfig;
use crate::control::Robot;

/// Embedding size (the model shortens its vectors to this; plenty for a home's worth of text).
const DIMS: usize = 256;
/// Days of diary and logs indexed (older ones are left out).
const DAYS: usize = 180;
/// Texts embedded per request.
const BATCH: usize = 64;
/// How often the background brings the index up to date.
const EVERY: Duration = Duration::from_secs(30 * 60);
/// What someone says brings this many things to mind at most, if they are at least this close.
const TO_MIND: usize = 3;
const TO_MIND_SCORE: f32 = 0.47;
/// ... and the search may take this long before the answer goes ahead without it.
const TO_MIND_WAIT: Duration = Duration::from_millis(800);
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    key: String,
    /// `memory`, `old memory`, `diary`, `day`, `talk` or `dream`.
    source: String,
    /// The day it is about, YYYY-MM-DD.
    date: String,
    text: String,
    /// Unit-length embedding, little-endian f32, base64.
    v: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub source: String,
    pub date: String,
    pub text: String,
    pub score: f32,
}

pub struct Recall {
    path: PathBuf,
    entries: Mutex<Vec<(Entry, Vec<f32>)>>,
    /// One update at a time.
    syncing: tokio::sync::Mutex<()>,
}

impl Recall {
    pub fn load(data_dir: &Path) -> Recall {
        let path = data_dir.join("recall.json");
        let entries: Vec<Entry> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let entries = entries
            .into_iter()
            .filter_map(|e| {
                let v = decode(&e.v)?;
                Some((e, v))
            })
            .collect();
        Recall {
            path,
            entries: Mutex::new(entries),
            syncing: tokio::sync::Mutex::new(()),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bring the index up to date with what it knows now; returns how many pieces were new.
    pub async fn sync(&self, robot: &Robot) -> Result<usize> {
        let _one = self.syncing.lock().await;
        let wanted = corpus(robot);
        let known: HashSet<String> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.key.clone())
            .collect();
        let new: Vec<(String, String, String, String)> = wanted
            .iter()
            .filter(|(key, ..)| !known.contains(key))
            .cloned()
            .collect();
        let mut added = Vec::new();
        for batch in new.chunks(BATCH) {
            let texts: Vec<String> = batch.iter().map(|(_, _, _, t)| t.clone()).collect();
            let vectors = embed(&robot.hub.config.brain, &texts).await?;
            for ((key, source, date, text), v) in batch.iter().cloned().zip(vectors) {
                let e = Entry {
                    key,
                    source,
                    date,
                    text,
                    v: encode(&v),
                };
                added.push((e, v));
            }
        }
        let keep: HashSet<&String> = wanted.iter().map(|(key, ..)| key).collect();
        let count = added.len();
        let changed = {
            let mut entries = self.entries.lock().unwrap();
            let before = entries.len();
            entries.retain(|(e, _)| keep.contains(&e.key));
            let removed = before - entries.len();
            entries.extend(added);
            count > 0 || removed > 0
        };
        if changed {
            self.save()?;
        }
        Ok(count)
    }

    /// The `k` pieces closest in meaning to `query`, at least `min` close, from `sources` (all if
    /// empty).
    pub async fn search(
        &self,
        robot: &Robot,
        query: &str,
        k: usize,
        min: f32,
        sources: &[&str],
    ) -> Result<Vec<Hit>> {
        if query.trim().is_empty() || self.is_empty() {
            return Ok(Vec::new());
        }
        let q = embed(&robot.hub.config.brain, &[query.to_string()])
            .await?
            .pop()
            .context("no embedding")?;
        // A memory forgotten since the last update must not come back.
        let facts: HashSet<String> = robot
            .memory
            .facts()
            .into_iter()
            .chain(robot.memory.archived())
            .map(|f| f.text)
            .collect();
        let entries = self.entries.lock().unwrap();
        let mut hits: Vec<Hit> = entries
            .iter()
            .filter(|(e, _)| sources.is_empty() || sources.contains(&e.source.as_str()))
            .filter(|(e, _)| !e.source.ends_with("memory") || facts.contains(&e.text))
            .map(|(e, v)| Hit {
                source: e.source.clone(),
                date: e.date.clone(),
                text: e.text.clone(),
                score: dot(&q, v),
            })
            .filter(|h| h.score >= min)
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }

    /// What `said` brings to mind, from what is not in its instructions already (its diary, past
    /// days, old memories), as JSON for an event. Nothing if the search is slow or fails.
    pub async fn brings_to_mind(&self, robot: &Robot, said: &str) -> Option<Value> {
        let search = self.search(
            robot,
            said,
            TO_MIND,
            TO_MIND_SCORE,
            &["old memory", "diary", "day", "talk", "dream"],
        );
        let hits = match tokio::time::timeout(TO_MIND_WAIT, search).await {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                tracing::debug!("recall: {e:#}");
                return None;
            }
            Err(_) => return None,
        };
        (!hits.is_empty()).then(|| hits_json(&hits))
    }

    fn save(&self) -> Result<()> {
        let entries: Vec<Entry> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.clone())
            .collect();
        crate::config::write_private(&self.path, &serde_json::to_vec(&entries)?)
    }
}

/// Hits as the agent sees them.
pub fn hits_json(hits: &[Hit]) -> Value {
    hits.iter()
        .map(|h| json!({ "when": h.date, "from": h.source, "text": h.text }))
        .collect::<Vec<_>>()
        .into()
}

/// Keep the index up to date. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    // Let the rest start first.
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        match robot.recall.sync(&robot).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("recall: {n} new pieces ({} in all)", robot.recall.len()),
            Err(e) => tracing::warn!("recall: {e:#}"),
        }
        tokio::time::sleep(EVERY).await;
    }
}

/// Everything to index: (key, source, date, text).
fn corpus(robot: &Robot) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    let mut add = |source: &str, date: &str, text: &str| {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let key: String = Sha256::digest(format!("{source}\n{date}\n{text}").as_bytes())
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push((key, source.to_string(), date.to_string(), text.to_string()));
    };
    for f in robot.memory.facts() {
        add("memory", &f.date, &f.text);
    }
    for f in robot.memory.archived() {
        add("old memory", &f.date, &f.text);
    }
    for day in robot.diary.days().into_iter().take(DAYS) {
        let entry = robot.diary.read(day).unwrap_or_default();
        for chunk in paragraphs(&entry, 600) {
            add("diary", &day.to_string(), &chunk);
        }
    }
    for day in robot.diary.logged_days().into_iter().take(DAYS) {
        for (source, text) in robot.diary.recall_pieces(day) {
            add(source, &day.to_string(), &text);
        }
    }
    // The same text twice (a fact remembered twice) is one piece.
    let mut seen = HashSet::new();
    out.retain(|(key, ..)| seen.insert(key.clone()));
    out
}

/// `text` in pieces of whole paragraphs, up to about `max` characters each.
fn paragraphs(text: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        match out.last_mut() {
            Some(last) if last.len() + p.len() < max => {
                last.push_str("\n\n");
                last.push_str(p);
            }
            _ => out.push(p.to_string()),
        }
    }
    out
}

async fn embed(cfg: &BrainConfig, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let key = cfg.api_key()?;
    let r: Value = client()
        .post(format!("{API}/embeddings"))
        .bearer_auth(key)
        .json(&json!({ "model": cfg.embedding_model, "input": texts, "dimensions": DIMS }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = r["error"]["message"].as_str() {
        bail!("embeddings: {e}");
    }
    let mut data: Vec<(u64, Vec<f32>)> = r["data"]
        .as_array()
        .context("embeddings: no data")?
        .iter()
        .map(|d| {
            let v: Vec<f32> = d["embedding"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_f64().map(|x| x as f32))
                .collect();
            (d["index"].as_u64().unwrap_or(0), unit(v))
        })
        .collect();
    data.sort_by_key(|(i, _)| *i);
    if data.len() != texts.len() {
        bail!("embeddings: {} for {} texts", data.len(), texts.len());
    }
    Ok(data.into_iter().map(|(_, v)| v).collect())
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn encode(v: &[f32]) -> String {
    B64.encode(v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>())
}

fn decode(s: &str) -> Option<Vec<f32>> {
    let b = B64.decode(s).ok()?;
    let (words, rest) = b.as_chunks::<4>();
    rest.is_empty()
        .then(|| words.iter().map(|w| f32::from_le_bytes(*w)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_survive_storage() {
        let v = unit(vec![3.0, 4.0, 0.0]);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
        assert_eq!(decode(&encode(&v)).unwrap(), v);
    }

    #[test]
    fn diary_entries_split_at_paragraphs() {
        let text = format!(
            "{}\n\n{}\n\n{}",
            "a".repeat(300),
            "b".repeat(200),
            "c".repeat(400)
        );
        let p = paragraphs(&text, 600);
        assert_eq!(p.len(), 2, "a and b fit together, c does not");
        assert!(p[0].starts_with('a') && p[0].ends_with('b'));
        assert!(paragraphs("  \n\n ", 600).is_empty());
    }
}
