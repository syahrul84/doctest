//! Facts for setting the system up on a server: runtime versions, PHP extensions, env keys the code reads, services
//! that must run (queue workers, scheduler, websockets), and the repo's deploy files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ServerFacts {
    pub php: Option<String>,
    pub laravel: Option<String>,
    pub node: Option<String>,
    pub php_extensions: Vec<String>,
    pub env_keys: Vec<EnvKey>,
    pub services: Vec<String>,
    pub deploy_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct EnvKey {
    pub name: String,
    pub default: Option<String>,
    pub used_in: Vec<String>,
    pub in_example: bool,
}

static ENV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"env\(\s*['"]([A-Z0-9_]+)['"]\s*(?:,\s*([^)]*?))?\s*\)"#).unwrap());

fn read_json(p: &Path) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['\'', '"']).to_string()
}

pub fn facts(cfg: &Config) -> ServerFacts {
    let be = &cfg.backend;
    let composer = read_json(&be.join("composer.json"));
    let req = &composer["require"];
    let mut php_extensions: Vec<String> = req
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, _)| k.strip_prefix("ext-").map(String::from))
        .collect();
    php_extensions.sort();
    let node = cfg.frontends.iter().find_map(|fe| {
        read_json(&fe.join("package.json"))["engines"]["node"]
            .as_str()
            .map(String::from)
            .or_else(|| {
                std::fs::read_to_string(fe.join(".nvmrc"))
                    .ok()
                    .map(|s| s.trim().to_string())
            })
    });

    let example: BTreeMap<String, String> = std::fs::read_to_string(be.join(".env.example"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            l.split_once('=').map(|(k, v)| (k.trim().to_string(), unquote(v)))
        })
        .collect();
    let mut keys: BTreeMap<String, EnvKey> = BTreeMap::new();
    let mut config_files: Vec<PathBuf> = std::fs::read_dir(be.join("config"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "php"))
        .collect();
    config_files.sort();
    for f in &config_files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let rel = format!("config/{}", f.file_name().unwrap_or_default().to_string_lossy());
        for c in ENV.captures_iter(&text) {
            let k = keys.entry(c[1].to_string()).or_insert_with(|| EnvKey {
                name: c[1].to_string(),
                ..EnvKey::default()
            });
            if k.default.is_none() {
                k.default = c
                    .get(2)
                    .map(|d| unquote(d.as_str()))
                    .filter(|d| !d.is_empty() && d != "null");
            }
            if !k.used_in.contains(&rel) {
                k.used_in.push(rel.clone());
            }
        }
    }
    for name in example.keys() {
        keys.entry(name.clone())
            .or_insert_with(|| EnvKey {
                name: name.clone(),
                ..EnvKey::default()
            })
            .in_example = true;
    }
    let value = |key: &str| {
        example
            .get(key)
            .cloned()
            .or_else(|| keys.get(key).and_then(|k| k.default.clone()))
    };

    let mut services = Vec::new();
    if let Some(q) = value("QUEUE_CONNECTION")
        .or_else(|| value("QUEUE_DRIVER"))
        .filter(|q| q != "sync" && !q.is_empty())
    {
        services.push(format!("queue: {q}"));
    }
    let console = std::fs::read_to_string(be.join("routes/console.php")).unwrap_or_default();
    let kernel = std::fs::read_to_string(be.join("app/Console/Kernel.php")).unwrap_or_default();
    if console.contains("Schedule::") || kernel.contains("$schedule->") {
        services.push("scheduler".into());
    }
    if let Some(b) = value("BROADCAST_CONNECTION")
        .or_else(|| value("BROADCAST_DRIVER"))
        .filter(|b| !matches!(b.as_str(), "" | "log" | "null"))
    {
        services.push(format!("broadcasting: {b}"));
    }
    if let Some(c) = value("CACHE_STORE")
        .or_else(|| value("CACHE_DRIVER"))
        .filter(|c| !c.is_empty())
    {
        services.push(format!("cache: {c}"));
    }
    if std::fs::read_to_string(be.join("config/filesystems.php"))
        .unwrap_or_default()
        .contains("'links'")
    {
        services.push("storage link".into());
    }

    let mut deploy_files = Vec::new();
    for dir in ["deploy", ".github/workflows"] {
        collect_rel(be, &be.join(dir), &mut deploy_files);
    }
    for f in ["Dockerfile", ".gitlab-ci.yml", "Procfile", "supervisord.conf"] {
        if be.join(f).is_file() {
            deploy_files.push(PathBuf::from(f));
        }
    }
    deploy_files.sort();

    ServerFacts {
        php: req["php"].as_str().map(String::from),
        laravel: req["laravel/framework"].as_str().map(String::from),
        node,
        php_extensions,
        env_keys: keys.into_values().collect(),
        services,
        deploy_files,
    }
}

fn collect_rel(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rel(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_path_buf());
        }
    }
}

/// Env keys whose name does not appear in the given documentation text.
pub fn undocumented(facts: &ServerFacts, docs: &str) -> Vec<String> {
    facts
        .env_keys
        .iter()
        .filter(|k| !docs.contains(&k.name))
        .map(|k| k.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::tests::shop_config;

    #[test]
    fn server_facts_from_fixture() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let f = facts(&cfg);
        assert_eq!(f.php.as_deref(), Some("^8.5"));
        assert_eq!(f.laravel.as_deref(), Some("^13.0"));
        assert_eq!(f.node.as_deref(), Some(">=24"));
        assert_eq!(f.php_extensions, vec!["gd", "intl"]);
        let mail = f.env_keys.iter().find(|k| k.name == "MAIL_HOST").unwrap();
        assert_eq!(mail.default.as_deref(), Some("smtp.example.com"));
        assert!(!mail.in_example);
        assert_eq!(mail.used_in, vec!["config/mail.php"]);
        assert!(f.env_keys.iter().any(|k| k.name == "DB_HOST" && k.in_example));
        for s in ["queue: redis", "scheduler", "broadcasting: reverb"] {
            assert!(f.services.iter().any(|x| x == s), "{s} not in {:?}", f.services);
        }
        assert_eq!(f.deploy_files, vec![PathBuf::from("deploy/nginx.conf")]);
    }

    #[test]
    fn undocumented_lists_missing_keys() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let f = facts(&cfg);
        let missing = undocumented(
            &f,
            "Set APP_NAME, APP_URL, DB_HOST and QUEUE_CONNECTION. BROADCAST_CONNECTION too.",
        );
        assert_eq!(missing, vec!["MAIL_HOST"]);
    }
}
