//! Model files downloaded on first use and checked against pinned SHA-256 hashes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub struct ModelFile {
    /// File name in the models directory.
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Refuse downloads larger than this.
    pub max_bytes: u64,
}

/// Path of `m` in `dir`, downloading and verifying it first if needed.
pub fn fetch(dir: &Path, m: &ModelFile) -> Result<PathBuf> {
    let path = dir.join(m.name);
    if let Ok(bytes) = std::fs::read(&path) {
        if sha256_hex(&bytes) == m.sha256 {
            return Ok(path);
        }
        tracing::warn!("{} is damaged; downloading it again", path.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    tracing::info!("downloading {}", m.url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();
    let mut bytes = Vec::new();
    agent
        .get(m.url)
        .call()
        .with_context(|| format!("downloading {}", m.url))?
        .into_body()
        .into_reader()
        .take(m.max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("downloading {}", m.url))?;
    if bytes.len() as u64 > m.max_bytes {
        bail!("{} is larger than expected; not using it", m.url);
    }
    let got = sha256_hex(&bytes);
    if got != m.sha256 {
        bail!(
            "{} has SHA-256 {got}, expected {}; not using it",
            m.url,
            m.sha256
        );
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
