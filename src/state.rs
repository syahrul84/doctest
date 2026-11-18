//! What doctest remembers between runs: `document/.doctest/state.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Last documented commit per repo (folder name → sha).
    pub last_commit: BTreeMap<String, String>,
    pub pages: BTreeMap<String, PageState>,
    pub ledger: Vec<Covered>,
    pub shots: BTreeMap<String, ShotSpec>,
    /// `<lang>/<source path>` → hash of the primary body it was translated from.
    pub translations: BTreeMap<String, String>,
    pub runs: Vec<RunRecord>,
    /// Tests doctest wrote that failed on their last run (`Class::method`).
    pub failing: Vec<String>,
    /// Work items handed out by `claim`, in plan order.
    pub queue: Vec<String>,
    /// Item → (worker, unix seconds when claimed).
    pub claims: BTreeMap<String, (String, u64)>,
    /// Item → why it is queued.
    pub reasons: BTreeMap<String, String>,
    /// Items written since the last finish (for the changelog).
    pub written: Vec<String>,
    /// When the current run started (unix seconds): the first status, plan or claim after a finish.
    pub run_started: Option<u64>,
}

/// A lock held by creating a file; removed on drop. A lock older than two minutes is taken over.
pub struct FileLock(PathBuf);

impl FileLock {
    pub fn acquire(path: PathBuf) -> Result<FileLock, String> {
        FileLock::acquire_with(path, 120)
    }

    /// `stale_secs`: a lock file older than this is taken over (a crashed holder).
    pub fn acquire_with(path: PathBuf, stale_secs: u64) -> Result<FileLock, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        // the same lock however the folder is spelled (symlinks such as /var → /private/var)
        let path = match (
            path.parent().and_then(|d| std::fs::canonicalize(d).ok()),
            path.file_name(),
        ) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => path,
        };
        let started = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(FileLock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let old = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > stale_secs);
                    if old {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed().as_secs() > stale_secs.max(600) {
                        return Err(format!(
                            "waited too long for {}; remove it if no doctest is running",
                            path.display()
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => return Err(format!("cannot lock {}: {e}", path.display())),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub const CLAIM_SECONDS: u64 = 30 * 60;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PageState {
    pub fingerprint: String,
    pub documented_fingerprint: Option<String>,
    pub shots: Vec<String>,
    /// Frontend files the page is built from (relative to its app).
    pub files: Vec<String>,
    /// Backend files of the actions it calls.
    pub backend_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Covered {
    pub target: String,
    pub case: String,
    pub test: String,
    pub file: PathBuf,
    pub source: CoverSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bug: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverSource {
    Doctest,
    Parity,
    Inferred,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunRecord {
    pub at: String,
    pub commits: BTreeMap<String, String>,
    pub updated: Vec<String>,
    pub note: String,
    /// How long the run took, from its first status/plan/claim to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
}

pub use crate::shots::ShotSpec;

impl State {
    pub fn path(cfg: &Config) -> PathBuf {
        cfg.out.join(".doctest/state.json")
    }

    /// Like load, but a state.json that exists and does not parse is an error (it is never overwritten).
    pub fn load_strict(cfg: &Config) -> Result<State, String> {
        let path = State::path(cfg);
        match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| {
                format!(
                    "{} does not parse ({e}); fix or remove it — doctest will not overwrite it",
                    path.display()
                )
            }),
            Err(_) => Ok(State::default()),
        }
    }

    pub fn load(cfg: &Config) -> State {
        std::fs::read_to_string(State::path(cfg))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Load, change and save under the state lock, so parallel workers do not overwrite each other.
    pub fn update<T>(cfg: &Config, f: impl FnOnce(&mut State) -> T) -> Result<T, String> {
        let _lock = FileLock::acquire(cfg.out.join(".doctest/state.lock"))?;
        let mut st = State::load_strict(cfg)?;
        let out = f(&mut st);
        st.save(cfg)?;
        Ok(out)
    }

    /// Takes what a plan learned (fingerprints, files, coverage ledger) without losing what others wrote meanwhile.
    pub fn merge_plan(&mut self, fresh: &State) {
        for (k, p) in &fresh.pages {
            let e = self.pages.entry(k.clone()).or_default();
            e.fingerprint = p.fingerprint.clone();
            e.files = p.files.clone();
            e.backend_files = p.backend_files.clone();
            if e.documented_fingerprint.is_none() {
                e.documented_fingerprint = p.documented_fingerprint.clone();
            }
            for s in &p.shots {
                if !e.shots.contains(s) {
                    e.shots.push(s.clone());
                }
            }
        }
        self.ledger = fresh.ledger.clone();
    }

    /// Up to `count` queued items not claimed by another worker (claims expire after 30 minutes).
    pub fn claim(&mut self, worker: &str, count: usize, now: u64) -> Vec<String> {
        let mut out = Vec::new();
        for id in &self.queue {
            if out.len() >= count {
                break;
            }
            let free = match self.claims.get(id) {
                None => true,
                Some((w, at)) => w == worker || now.saturating_sub(*at) > CLAIM_SECONDS,
            };
            if free {
                out.push(id.clone());
            }
        }
        for id in &out {
            self.claims.insert(id.clone(), (worker.to_string(), now));
        }
        out
    }

    /// An item was written: it leaves the queue and its claim is released.
    pub fn done(&mut self, item: &str) {
        self.queue.retain(|q| q != item);
        self.claims.remove(item);
        self.reasons.remove(item);
        if !self.written.iter().any(|w| w == item) {
            self.written.push(item.to_string());
        }
    }

    /// Test runs one at a time per project (they share a test database).
    pub fn test_lock(cfg: &Config) -> Result<FileLock, String> {
        FileLock::acquire_with(cfg.out.join(".doctest/tests.lock"), 3600)
    }

    pub fn save(&self, cfg: &Config) -> Result<(), String> {
        let path = State::path(cfg);
        let dir = path.parent().expect("state path has a parent");
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())? + "\n";
        let tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        std::fs::write(tmp.path(), text).map_err(|e| e.to_string())?;
        tmp.persist(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &std::path::Path) -> Config {
        std::fs::write(dir.join(".doctest.toml"), "[project]\nname=\"x\"\n").unwrap();
        Config::find(dir).unwrap()
    }

    #[test]
    fn concurrent_updates_all_land() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        std::thread::scope(|s| {
            for i in 0..8 {
                let c = &c;
                s.spawn(move || {
                    State::update(c, |st| {
                        st.pages.insert(format!("p{i}"), PageState::default());
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(State::load(&c).pages.len(), 8);
    }

    #[test]
    fn merge_plan_keeps_documented_and_shots() {
        let mut saved = State::default();
        saved.pages.insert(
            "p".into(),
            PageState {
                fingerprint: "old".into(),
                documented_fingerprint: Some("old".into()),
                shots: vec!["s".into()],
                ..PageState::default()
            },
        );
        let mut fresh = State::default();
        fresh.pages.insert(
            "p".into(),
            PageState {
                fingerprint: "new".into(),
                files: vec!["a.vue".into()],
                ..PageState::default()
            },
        );
        fresh.pages.insert(
            "q".into(),
            PageState {
                fingerprint: "q1".into(),
                ..PageState::default()
            },
        );
        saved.merge_plan(&fresh);
        assert_eq!(saved.pages["p"].fingerprint, "new");
        assert_eq!(saved.pages["p"].documented_fingerprint.as_deref(), Some("old"));
        assert_eq!(saved.pages["p"].shots, vec!["s"]);
        assert_eq!(saved.pages["p"].files, vec!["a.vue"]);
        assert!(saved.pages.contains_key("q"));
    }

    #[test]
    fn claims_are_disjoint_and_expire() {
        let mut st = State {
            queue: (0..5).map(|i| format!("guide:en:p{i}")).collect(),
            ..State::default()
        };
        let a = st.claim("w1", 2, 1000);
        let b = st.claim("w2", 2, 1000 + 20 * 60);
        assert_eq!(a, vec!["guide:en:p0", "guide:en:p1"]);
        assert_eq!(b, vec!["guide:en:p2", "guide:en:p3"]);
        st.done("guide:en:p0");
        assert!(!st.queue.contains(&"guide:en:p0".to_string()) && !st.claims.contains_key("guide:en:p0"));
        // 31 minutes after w1 claimed, its remaining claim has expired; w2 claimed 11 minutes ago
        let c = st.claim("w3", 5, 1000 + 31 * 60);
        assert_eq!(c, vec!["guide:en:p1", "guide:en:p4"]);
    }

    #[test]
    fn broken_state_is_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        std::fs::create_dir_all(c.out.join(".doctest")).unwrap();
        std::fs::write(State::path(&c), "{\"last_commit\": {\"a\": \"b\"},}").unwrap();
        let err = State::update(&c, |s| s.queue.push("x".into())).unwrap_err();
        assert!(err.contains("state.json"), "{err}");
        assert!(
            std::fs::read_to_string(State::path(&c))
                .unwrap()
                .contains("\"a\": \"b\"")
        );
    }

    #[test]
    fn missing_state_is_default() {
        let d = tempfile::tempdir().unwrap();
        let s = State::load(&cfg(d.path()));
        assert!(s.last_commit.is_empty());
        assert!(s.ledger.is_empty());
    }

    #[test]
    fn state_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        let mut s = State::default();
        s.last_commit.insert("shop-api".into(), "abc".into());
        s.pages.insert(
            "p".into(),
            PageState {
                fingerprint: "f".into(),
                documented_fingerprint: Some("f".into()),
                shots: vec!["m/p-1".into()],
                ..PageState::default()
            },
        );
        s.ledger.push(Covered {
            target: "POST api/x".into(),
            case: "guest".into(),
            test: "T::t".into(),
            file: "tests/T.php".into(),
            source: CoverSource::Parity,
            bug: None,
        });
        s.save(&c).unwrap();
        assert!(c.out.join(".doctest/state.json").is_file());
        assert_eq!(State::load(&c), s);
    }
}
