//! Page facts kept between runs, so `status`, `plan` and `claim` read only the pages whose code changed. A page's
//! facts are reused while the files they came from (frontend components, backend controllers) are unchanged and
//! nothing global changed (doctest's version, the locale files, the menu, the routes, .doctest.toml).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::pages::PageFacts;

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    global: String,
    pages: BTreeMap<String, Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    files: String,
    facts: PageFacts,
}

pub struct FactCache {
    path: PathBuf,
    global: String,
    stored: Stored,
    file_hashes: HashMap<PathBuf, String>,
    changed: bool,
    pub hits: usize,
}

fn path_for(cfg: &Config) -> PathBuf {
    if cfg!(test) {
        return cfg.out.join(".doctest/cache/facts.json");
    }
    let id = crate::hash::sha256(&cfg.root.to_string_lossy());
    crate::reports::tool_root()
        .join("cache")
        .join(&id[..12])
        .join("facts.json")
}

fn read(path: &std::path::Path) -> String {
    std::fs::read(path)
        .map(|b| crate::hash::hex(&sha2_digest(&b)))
        .unwrap_or_else(|_| "-".into())
}

fn sha2_digest(b: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).to_vec()
}

/// Everything that changes every page's facts at once.
fn global_key(cfg: &Config, labels_hash: &str) -> String {
    let mut parts = vec![
        crate::update::LONG_VERSION.to_string(),
        labels_hash.to_string(),
        read(&cfg.root.join(crate::config::FILE)),
    ];
    if let Some(m) = &cfg.guide.modules_from {
        parts.push(read(m));
    }
    let mut routes: Vec<PathBuf> = std::fs::read_dir(cfg.backend.join("routes"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "php"))
        .collect();
    routes.sort();
    for r in routes {
        parts.push(read(&r));
    }
    crate::hash::sha256(&parts.join("\n"))
}

impl FactCache {
    pub fn load(cfg: &Config, labels_hash: &str) -> FactCache {
        let path = path_for(cfg);
        let global = global_key(cfg, labels_hash);
        let stored: Stored = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(|s: &Stored| s.global == global)
            .unwrap_or_default();
        FactCache {
            path,
            global,
            stored,
            file_hashes: HashMap::new(),
            changed: false,
            hits: 0,
        }
    }

    /// The repo a page's code lives in.
    fn app_root(cfg: &Config, app: &str) -> PathBuf {
        cfg.frontends
            .iter()
            .find(|f| Config::repo_name(f) == app)
            .cloned()
            .unwrap_or_else(|| cfg.backend.clone())
    }

    fn files_key(&mut self, cfg: &Config, f: &PageFacts) -> String {
        let root = Self::app_root(cfg, &f.page.app);
        let mut files: Vec<PathBuf> = f.code_files.iter().map(|c| root.join(c)).collect();
        files.extend(f.endpoints.iter().filter_map(|e| {
            e.at.as_deref()
                .and_then(|a| a.rsplit_once(':'))
                .map(|(file, _)| cfg.backend.join(file))
        }));
        if files.is_empty() {
            // a page whose files are unknown is always read again
            return String::new();
        }
        files.sort();
        files.dedup();
        let hashes: Vec<String> = files
            .iter()
            .map(|p| self.file_hashes.entry(p.clone()).or_insert_with(|| read(p)).clone())
            .collect();
        crate::hash::sha256(&hashes.join("\n"))
    }

    /// The stored facts of a page, if the files they came from are unchanged.
    pub fn get(&mut self, cfg: &Config, key: &str) -> Option<PageFacts> {
        let e = self.stored.pages.get(key)?.clone();
        let now = self.files_key(cfg, &e.facts);
        if !now.is_empty() && now == e.files {
            self.hits += 1;
            Some(e.facts)
        } else {
            None
        }
    }

    pub fn put(&mut self, cfg: &Config, facts: &PageFacts) {
        let files = self.files_key(cfg, facts);
        self.stored.pages.insert(
            facts.page.key.clone(),
            Entry {
                files,
                facts: facts.clone(),
            },
        );
        self.changed = true;
    }

    pub fn save(&mut self) {
        if !self.changed {
            return;
        }
        self.stored.global = self.global.clone();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(t) = serde_json::to_string(&self.stored) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, t).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
        self.changed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_reused_until_a_file_changes() {
        let (_d, cfg) = crate::work::tests::project();
        std::fs::create_dir_all(cfg.backend.join("src")).unwrap();
        let file = cfg.backend.join("src/Page.vue");
        std::fs::write(&file, "<template>a</template>").unwrap();
        let mut f: PageFacts = serde_json::from_value(serde_json::json!({
            "page": {"key": "orders", "app": "nope", "path": "/orders", "route_name": "orders", "component": "Page", "module": "m"},
            "labels": {}, "literals": [], "components": [], "code_files": ["src/Page.vue"], "endpoints": [], "menu_titles": []
        }))
        .unwrap();
        f.page.app = Config::repo_name(&cfg.backend);
        let mut c = FactCache::load(&cfg, "labels");
        assert!(c.get(&cfg, "orders").is_none());
        c.put(&cfg, &f);
        c.save();
        let mut c = FactCache::load(&cfg, "labels");
        assert_eq!(c.get(&cfg, "orders").as_ref(), Some(&f));
        std::fs::write(&file, "<template>b</template>").unwrap();
        let mut c = FactCache::load(&cfg, "labels");
        assert!(c.get(&cfg, "orders").is_none(), "a changed file reads the page again");
        // new locale files change every page
        let mut c = FactCache::load(&cfg, "other labels");
        assert!(c.get(&cfg, "orders").is_none());
        c.put(&cfg, &f);
        assert!(c.get(&cfg, "orders").is_some());
    }
}
