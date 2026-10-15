//! The control API token. The CLI sends it from this Mac, over Tailscale, or
//! through the tunnel, so it is the one secret that guards deploys. Wrong
//! guesses are throttled, and the token can be rotated if it leaks.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::warn;

use crate::secrets::{constant_time_eq, random_token};

/// More wrong tokens than this in a minute and every wrong attempt is slowed
/// down. The right token always works, so nobody can lock you out.
const FAILURES_PER_MINUTE: usize = 20;
const WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Valid,
    Invalid,
    /// Invalid, and there have been too many wrong attempts recently.
    Throttled,
}

pub struct ApiToken {
    path: PathBuf,
    current: RwLock<String>,
    failures: Mutex<VecDeque<Instant>>,
    last_warning: Mutex<Option<Instant>>,
}

/// Reads the token file, creating it (mode 0600) the first time.
pub fn load_or_create(path: &Path) -> Result<String> {
    if let Ok(token) = std::fs::read_to_string(path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = random_token(32);
    write_private(path, &token)?;
    Ok(token)
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

impl ApiToken {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            current: RwLock::new(load_or_create(path)?),
            path: path.to_path_buf(),
            failures: Mutex::default(),
            last_warning: Mutex::default(),
        })
    }

    pub fn check(&self, presented: Option<&str>) -> Check {
        if presented.is_some_and(|token| constant_time_eq(token, &self.current.read().unwrap())) {
            return Check::Valid;
        }
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap();
        while failures
            .front()
            .is_some_and(|at| now.duration_since(*at) > WINDOW)
        {
            failures.pop_front();
        }
        failures.push_back(now);
        if failures.len() <= FAILURES_PER_MINUTE {
            return Check::Invalid;
        }
        let mut last_warning = self.last_warning.lock().unwrap();
        if last_warning.is_none_or(|at| now.duration_since(at) > WINDOW) {
            warn!(
                attempts = failures.len(),
                "many requests with a wrong API token in the last minute; if your token leaked, run `hedgehog token --rotate`"
            );
            *last_warning = Some(now);
        }
        Check::Throttled
    }

    /// Replaces the token with a new one and returns it. The old one stops
    /// working immediately.
    pub fn rotate(&self) -> Result<String> {
        let token = random_token(32);
        write_private(&self.path, &token)?;
        *self.current.write().unwrap() = token.clone();
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_and_throttles_wrong_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let token = ApiToken::load(&dir.path().join("token")).unwrap();
        let right = std::fs::read_to_string(dir.path().join("token")).unwrap();
        assert_eq!(token.check(Some(&right)), Check::Valid);
        assert_eq!(token.check(None), Check::Invalid);
        for _ in 0..FAILURES_PER_MINUTE {
            token.check(Some("guess"));
        }
        assert_eq!(token.check(Some("guess")), Check::Throttled);
        assert_eq!(
            token.check(Some(&right)),
            Check::Valid,
            "the right token always works"
        );
    }

    #[test]
    fn rotation_replaces_the_token_on_disk_and_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let token = ApiToken::load(&path).unwrap();
        let old = std::fs::read_to_string(&path).unwrap();
        let new = token.rotate().unwrap();
        assert_ne!(old, new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
        assert_eq!(token.check(Some(&old)), Check::Invalid);
        assert_eq!(token.check(Some(&new)), Check::Valid);
        assert_eq!(
            ApiToken::load(&path).unwrap().check(Some(&new)),
            Check::Valid
        );
    }
}
