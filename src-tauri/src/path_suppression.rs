//! Suppress filesystem paths we just created/moved so Watcher does not
//! re-ingest pipeline destinations under a recursive watch tree

use crate::path_template::normalize_db_path;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[allow(non_upper_case_globals)]
const default_ttl: Duration = Duration::from_secs(90);

/// Timed set of paths the pipeline produced Ignore matching notify events
#[derive(Debug)]
pub struct PathSuppression {
    inner: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
    /// Test/injected clock skew so expiry can be asserted without sleeping.
    clock_skew: Mutex<Duration>,
}

impl Default for PathSuppression {
    fn default() -> Self {
        Self::new(default_ttl)
    }
}

impl PathSuppression {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            clock_skew: Mutex::new(Duration::ZERO),
        }
    }

    /// Advance the logical clock (tests / deterministic expiry).
    #[cfg(any(test, feature = "test-support"))]
    pub fn advance_clock(&self, by: Duration) {
        if let Ok(mut skew) = self.clock_skew.lock() {
            *skew = skew.saturating_add(by);
        }
    }

    fn now(&self) -> Instant {
        let skew = self
            .clock_skew
            .lock()
            .map(|g| *g)
            .unwrap_or(Duration::ZERO);
        Instant::now() + skew
    }

    /// Remember a destination so subsequent notify events are ignored
    pub fn suppress(&self, path: &Path) {
        let mut keys = Vec::with_capacity(2);
        keys.push(normalize_key(path));
        if let Ok(canon) = path.canonicalize() {
            let c = normalize_key(&canon);
            if c != keys[0] {
                keys.push(c);
            }
        }
        let now = self.now();
        if let Ok(mut guard) = self.inner.lock() {
            Self::purge_locked(&mut guard, self.ttl, now);
            for key in keys {
                guard.insert(key, now);
            }
        }
    }

    /// True when `path` was recently suppressed
    pub fn is_suppressed(&self, path: &Path) -> bool {
        let now = self.now();
        let candidates = [
            normalize_key(path),
            path.canonicalize()
                .map(|p| normalize_key(&p))
                .unwrap_or_default(),
        ];
        let Ok(mut guard) = self.inner.lock() else {
            return false;
        };
        Self::purge_locked(&mut guard, self.ttl, now);
        candidates
            .iter()
            .any(|k| !k.is_empty() && guard.contains_key(k))
    }

    fn purge_locked(map: &mut HashMap<String, Instant>, ttl: Duration, now: Instant) {
        map.retain(|_, at| now.duration_since(*at) <= ttl);
    }
}

fn normalize_key(path: &Path) -> String {
    // Same rules as DB path strings so suppress matches ingest / template dest
    normalize_db_path(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "idam_supp_{}_{}_{}",
            label,
            std::process::id(),
            nanos
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn suppress_matches_same_path() {
        let dir = unique_temp_dir("match");
        let file = dir.join("clip.mp4");
        fs::write(&file, b"x").unwrap();
        let supp = PathSuppression::new(Duration::from_secs(30));
        assert!(!supp.is_suppressed(&file));
        supp.suppress(&file);
        assert!(supp.is_suppressed(&file));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn suppress_expires_after_ttl_via_clock_advance() {
        let dir = unique_temp_dir("ttl");
        let file = dir.join("clip.mp4");
        fs::write(&file, b"x").unwrap();
        // Use a generous TTL so canonicalize / CI scheduling cannot expire
        // the entry before the first is_suppressed check (30ms was flaky on Windows)
        let supp = PathSuppression::new(Duration::from_secs(30));
        supp.suppress(&file);
        assert!(supp.is_suppressed(&file));
        supp.advance_clock(Duration::from_secs(31));
        assert!(!supp.is_suppressed(&file));
        let _ = fs::remove_dir_all(&dir);
    }

    /// #61: suppress with `/` must match a query that uses `\`.
    #[cfg(windows)]
    #[test]
    fn suppress_slash_matches_backslash_query() {
        let with_slash = Path::new(r"C:/lib/StudioA/clip.mp4");
        let with_backslash = Path::new(r"C:\lib\StudioA\clip.mp4");
        let supp = PathSuppression::new(Duration::from_secs(30));
        supp.suppress(with_slash);
        assert!(
            supp.is_suppressed(with_backslash),
            "PathSuppression must normalize separators for lookup"
        );
    }
}
