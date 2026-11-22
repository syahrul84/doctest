//! What to do next: page guides missing or out of date, translations behind their primary guide, process guides,
//! screenshots to retake, system document sections, and test cases no test covers. Scope `changes` looks only at
//! what changed since the last finished run (per repo); `all` at the whole system.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use crate::cases::{self, Case};
use crate::codemap::{BACKEND, CodemapApi};
use crate::config::Config;
use crate::i18n::Labels;
use crate::pages::{Endpoint, FactBuilder, Page, PageFacts, endpoint_from_route, fingerprint};
use crate::source::{self, Kind, Source};
use crate::state::State;

#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    /// Since the given commits per repo, else since the last finished run.
    Changes {
        since: Option<BTreeMap<String, String>>,
    },
    /// What the current branch changed since it left `base` (default origin/HEAD, main, master, develop).
    Branch {
        base: Option<String>,
    },
    All,
}

impl Scope {
    pub fn parse(scope: Option<&str>, base: Option<&str>) -> Result<Scope, String> {
        match scope.unwrap_or("changes") {
            "all" => Ok(Scope::All),
            "changes" => Ok(Scope::Changes { since: None }),
            "branch" => Ok(Scope::Branch {
                base: base.map(String::from),
            }),
            other => Err(format!("scope must be changes, branch or all, not {other}")),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Changes { .. } => "changes",
            Scope::Branch { .. } => "branch",
        }
    }
}

/// The commits per repo a scope starts from (None = whole system / first run).
pub fn since_of(cfg: &Config, state: &State, scope: &Scope) -> Result<Option<BTreeMap<String, String>>, String> {
    Ok(match scope {
        Scope::All => None,
        Scope::Changes { since } => since
            .clone()
            .or_else(|| (!state.last_commit.is_empty()).then(|| state.last_commit.clone())),
        Scope::Branch { base } => {
            let mut m = BTreeMap::new();
            for repo in std::iter::once(&cfg.backend).chain(&cfg.frontends) {
                m.insert(Config::repo_name(repo), crate::git::merge_base(repo, base.as_deref())?);
            }
            Some(m)
        }
    })
}

/// A page key or path matches a pattern (`*` at the end matches the rest).
pub fn page_matches(key: &str, path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let m = |s: &str| match p.strip_suffix('*') {
            Some(prefix) => s.starts_with(prefix),
            None => s == p,
        };
        m(key) || m(path)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Guide,
    Translation,
    Process,
    /// `shots:<page>`: a written guide still short of the screenshots its page needs (text first, screenshots after)
    Shots,
    Shot,
    Doc,
    Tests,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkItem {
    pub id: String,
    pub kind: ItemKind,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Count {
    pub done: usize,
    pub total: usize,
    pub percent: u32,
}

impl Count {
    pub fn new(done: usize, total: usize) -> Count {
        Count {
            done,
            total,
            percent: crate::progress::percent(done, total),
        }
    }
}

/// How far the whole job is (whole system, whatever the scope).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Coverage {
    pub guides: Count,
    pub translations: Count,
    pub screenshots: Count,
    pub test_cases: Count,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub coverage: Coverage,
    pub items: Vec<WorkItem>,
    pub stale_index: bool,
    pub pages: Vec<Page>,
    pub facts: BTreeMap<String, PageFacts>,
    pub fingerprints: BTreeMap<String, String>,
    pub endpoints: Vec<Endpoint>,
    /// Target → cases no test covers.
    pub missing: BTreeMap<String, Vec<Case>>,
    /// Changed files per repo name (None in scope all / first run).
    pub changed: Option<BTreeMap<String, Vec<PathBuf>>>,
    /// Pages whose facts came from the cache (their code had not changed).
    pub cached_pages: usize,
}

pub const SYSTEM_SECTIONS: [(&str, &str); 5] = [
    (
        "system/architecture",
        "apps, how they talk, services (queue, cache, websockets, mail, storage)",
    ),
    (
        "system/configuration",
        "every env key the code reads: meaning, example, required",
    ),
    ("system/api", "endpoints: method, URI, permission, request rules"),
    ("system/database", "tables and columns, grouped by module"),
    ("system/jobs", "queued jobs, artisan commands, the schedule"),
];

/// Current commit of each repo, keyed by repo name.
pub fn commits(cfg: &Config) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for repo in std::iter::once(&cfg.backend).chain(&cfg.frontends) {
        out.insert(Config::repo_name(repo), crate::git::head(repo)?);
    }
    Ok(out)
}

/// Changed files per repo since the given commits.
pub fn changed_files(cfg: &Config, since: &BTreeMap<String, String>) -> Result<BTreeMap<String, Vec<PathBuf>>, String> {
    let mut out = BTreeMap::new();
    for repo in std::iter::once(&cfg.backend).chain(&cfg.frontends) {
        let name = Config::repo_name(repo);
        let files = match since.get(&name) {
            Some(c) => crate::git::changed_since(repo, c)?,
            None => Vec::new(),
        };
        out.insert(name, files);
    }
    Ok(out)
}

fn is_locale(p: &Path) -> bool {
    p.components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("locales" | "locale" | "lang" | "i18n")))
}

/// The languages whose guide of a page has fewer screenshots than the page needs (`en 1/3`); empty when none is short.
pub fn shots_short(cfg: &Config, sources: &[Source], key: &str, f: &crate::pages::PageFacts) -> Vec<String> {
    cfg.languages
        .iter()
        .filter_map(|lang| {
            let g = page_guide(sources, key, lang)?;
            let need = cfg.guide.min_screenshots.unwrap_or_else(|| {
                crate::pages::screenshot_plan_for(
                    &crate::pages::required_actions(f),
                    crate::pages::says_no_data(&g.body),
                )
                .len()
            });
            let have = guide_images(&g.body);
            (have < need).then(|| format!("{lang} {have}/{need}"))
        })
        .collect()
}

/// How many images a guide shows.
pub fn guide_images(body: &str) -> usize {
    static IMG: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"!\[[^\]]*\]\([^)\s]+\)").unwrap());
    IMG.find_iter(body).count()
}

/// The guide source for a page in a language.
pub fn page_guide<'a>(sources: &'a [Source], key: &str, lang: &str) -> Option<&'a Source> {
    sources
        .iter()
        .find(|s| s.fm.kind == Kind::Page && s.fm.lang == lang && s.fm.routes.first().map(String::as_str) == Some(key))
}

/// `guide/<from>/…` → `guide/<to>/…`.
pub fn swap_lang(path: &Path, to: &str) -> PathBuf {
    let mut parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() > 1 && parts[0] == "guide" {
        parts[1] = to.to_string();
    }
    PathBuf::from(parts.join("/"))
}

fn class_of_file(rel: &Path) -> Option<String> {
    let s = rel.to_string_lossy();
    let s = s.strip_prefix("app/")?.strip_suffix(".php")?;
    Some(format!("App\\{}", s.replace('/', "\\")))
}

/// Classes worth unit tests: app/ PHP outside controllers, models, providers, console kernel, requests, resources.
fn unit_candidate(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    s.starts_with("app/")
        && s.ends_with(".php")
        && ![
            "app/Http/",
            "app/Models/",
            "app/Providers/",
            "app/Console/Kernel.php",
            "app/Exceptions/",
        ]
        .iter()
        .any(|p| s.starts_with(p))
}

#[cfg(test)]
pub fn plan(cfg: &Config, api: &mut dyn CodemapApi, state: &mut State, scope: &Scope) -> Result<Plan, String> {
    plan_with(cfg, api, state, scope, &crate::progress::Silent)
}

pub fn plan_with(
    cfg: &Config,
    api: &mut dyn CodemapApi,
    state: &mut State,
    scope: &Scope,
    progress: &dyn crate::progress::Progress,
) -> Result<Plan, String> {
    plan_parallel(cfg, api, state, scope, progress, None)
}

/// Makes one more codemap client, for a parallel worker.
pub type ApiFactory<'a> = dyn Fn() -> Box<dyn CodemapApi + Send> + Sync + 'a;

/// Runs `work` over `items` in up to `workers` threads, each with its own codemap client, keeping input order.
pub fn in_parallel<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    factory: &ApiFactory<'_>,
    progress: &dyn crate::progress::Progress,
    what: &str,
    work: &(dyn Fn(&mut dyn CodemapApi, &T) -> R + Sync),
) -> Vec<R> {
    let total = items.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let size = total.div_ceil(workers.max(1)).max(1);
    let mut out: Vec<(usize, R)> = Vec::with_capacity(total);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(size)
            .enumerate()
            .map(|(c, chunk)| {
                let done = &done;
                s.spawn(move || {
                    let mut api = factory();
                    chunk
                        .iter()
                        .enumerate()
                        .map(|(i, item)| {
                            let r = work(&mut *api, item);
                            let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            progress.step(n, total, what);
                            (c * size + i, r)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            if let Ok(v) = h.join() {
                out.extend(v);
            }
        }
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

pub fn plan_parallel(
    cfg: &Config,
    api: &mut dyn CodemapApi,
    state: &mut State,
    scope: &Scope,
    progress: &dyn crate::progress::Progress,
    factory: Option<&ApiFactory<'_>>,
) -> Result<Plan, String> {
    let sources = source::sources(cfg)?;
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let primary = cfg.primary_language().to_string();
    let mut fb = FactBuilder::new(api, cfg, &labels);
    let mut plan = Plan::default();
    let pages = fb.pages()?;

    let since = since_of(cfg, state, scope)?;
    let first_run = matches!(scope, Scope::Changes { .. }) && since.is_none();
    let changed = match &since {
        Some(s) => Some(changed_files(cfg, s)?),
        None => None,
    };
    let backend_name = Config::repo_name(&cfg.backend);
    let changed_backend: Vec<PathBuf> = changed
        .as_ref()
        .and_then(|c| c.get(&backend_name).cloned())
        .unwrap_or_default();
    let locale_changed = changed
        .as_ref()
        .is_some_and(|c| c.values().flatten().any(|p| is_locale(p)));

    // pages to look at, their facts and whether their guide is behind
    let mut stale_pages: BTreeMap<String, String> = BTreeMap::new(); // key → reason
    let mut candidates: Vec<&Page> = Vec::new();
    for page in &pages {
        let prev = state.pages.get(&page.key).cloned();
        let candidate = match (&changed, &prev) {
            (None, _) | (_, None) => true,
            (Some(ch), Some(prev)) => {
                let fe: Vec<String> = ch
                    .get(&page.app)
                    .into_iter()
                    .flatten()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let be: Vec<String> = changed_backend
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                locale_changed
                    || prev.files.iter().any(|f| fe.contains(f))
                    || prev.backend_files.iter().any(|f| be.contains(f))
            }
        };
        if candidate {
            candidates.push(page);
        }
    }
    // pages whose code is unchanged since they were last read keep their facts
    let mut cache = crate::factcache::FactCache::load(cfg, labels.files_hash());
    let cached: Vec<Option<PageFacts>> = candidates
        .iter()
        .map(|p| cache.get(cfg, &p.key).filter(|f| f.page == **p))
        .collect();
    let to_read: Vec<&Page> = candidates
        .iter()
        .zip(&cached)
        .filter(|(_, c)| c.is_none())
        .map(|(p, _)| *p)
        .collect();
    progress.step(0, to_read.len(), "reading pages");
    let read: Vec<Result<PageFacts, String>> = match factory {
        Some(f) if cfg.parallel_reads && cfg.workers > 1 && to_read.len() > 1 => {
            in_parallel(&to_read, cfg.workers, f, progress, "reading pages", &|api, page| {
                let mut fb = FactBuilder::new(api, cfg, &labels);
                fb.facts(page)
            })
        }
        _ => to_read
            .iter()
            .enumerate()
            .map(|(n, page)| {
                let r = fb.facts(page);
                progress.step(n + 1, to_read.len(), "reading pages");
                r
            })
            .collect(),
    };
    let mut read = read.into_iter();
    let mut computed: Vec<Result<PageFacts, String>> = Vec::new();
    for c in cached {
        computed.push(match c {
            Some(f) => Ok(f),
            None => {
                let r = read.next().expect("one read per uncached page");
                if let Ok(f) = &r {
                    cache.put(cfg, f);
                }
                r
            }
        });
    }
    cache.save();
    plan.cached_pages = cache.hits;
    for (page, facts) in candidates.iter().zip(computed) {
        let facts = facts?;
        let fp = fingerprint(&facts, labels.files_hash());
        let entry = state.pages.entry(page.key.clone()).or_default();
        if crate::pages::carries_over(entry.documented_fingerprint.as_deref(), &entry.fingerprint) {
            entry.documented_fingerprint = Some(fp.clone());
        }
        entry.fingerprint = fp.clone();
        entry.files = facts.code_files.clone();
        entry.backend_files = facts
            .endpoints
            .iter()
            .filter_map(|e| {
                e.at.as_deref()
                    .and_then(|a| a.rsplit_once(':'))
                    .map(|(f, _)| f.to_string())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let documented = entry.documented_fingerprint.clone();
        let has_guide = page_guide(&sources, &page.key, &primary).is_some();
        let reason = if !has_guide {
            Some(if first_run {
                "first run: no guide yet".to_string()
            } else {
                "no guide yet".to_string()
            })
        } else if documented.as_deref() != Some(fp.as_str()) {
            Some(match documented {
                None => "the guide was never checked against this page".to_string(),
                Some(_) => {
                    "the page changed since the guide was written (fields, labels, endpoints or messages)".to_string()
                }
            })
        } else {
            None
        };
        if let Some(r) = reason {
            stale_pages.insert(page.key.clone(), r);
        }
        plan.fingerprints.insert(page.key.clone(), fp);
        plan.facts.insert(page.key.clone(), facts);
    }
    for (key, reason) in &stale_pages {
        plan.items.push(WorkItem {
            id: format!("guide:{primary}:{key}"),
            kind: ItemKind::Guide,
            reason: reason.clone(),
        });
    }

    // translations of every primary guide
    for s in sources
        .iter()
        .filter(|s| s.path.starts_with("guide") && s.fm.lang == primary)
    {
        let name =
            s.fm.routes
                .first()
                .cloned()
                .filter(|_| s.fm.kind == Kind::Page)
                .unwrap_or_else(|| source::slug(&s.fm.title));
        for lang in cfg.languages.iter().filter(|l| **l != primary) {
            let other = swap_lang(&s.path, lang);
            let reason = match sources.iter().find(|x| x.path == other) {
                None => Some(format!("no {lang} translation")),
                Some(t) if !source::translated_from_current(t.fm.translated_from.as_deref(), &s.body) => {
                    Some(format!("the {primary} guide changed since this translation"))
                }
                Some(_) => None,
            };
            if let Some(r) = reason {
                plan.items.push(WorkItem {
                    id: format!("translation:{lang}:{name}"),
                    kind: ItemKind::Translation,
                    reason: r,
                });
            }
        }
    }

    // process guides whose pages changed; suggestions in scope all
    for s in sources
        .iter()
        .filter(|s| s.fm.kind == Kind::Process && s.fm.lang == primary)
    {
        let hit: Vec<&String> =
            s.fm.routes
                .iter()
                .chain(&s.fm.covers)
                .filter(|k| stale_pages.contains_key(*k))
                .collect();
        if !hit.is_empty() {
            plan.items.push(WorkItem {
                id: format!("process:{primary}:{}", source::slug(&s.fm.title)),
                kind: ItemKind::Process,
                reason: format!(
                    "covered pages changed: {}",
                    hit.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ),
            });
        }
    }
    if *scope == Scope::All {
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        for m in &fb.menu {
            *groups.entry(format!("{} / {}", m.module, m.group)).or_default() += 1;
        }
        let big: Vec<String> = groups
            .into_iter()
            .filter(|(_, n)| *n >= 3)
            .map(|(g, n)| format!("{g} ({n} pages)"))
            .collect();
        if !big.is_empty() && !sources.iter().any(|s| s.fm.kind == Kind::Process) {
            plan.items.push(WorkItem {
                id: format!("process:{primary}:suggest"),
                kind: ItemKind::Process,
                reason: format!("no process guides yet; candidate menu groups: {}", big.join("; ")),
            });
        }
    }

    // text first: written guides still short of their screenshots (not while the guide itself is to be rewritten)
    for page in &pages {
        if stale_pages.contains_key(&page.key) {
            continue;
        }
        let Some(f) = plan.facts.get(&page.key) else { continue };
        let short = shots_short(cfg, &sources, &page.key, f);
        if !short.is_empty() {
            plan.items.push(WorkItem {
                id: format!("shots:{}", page.key),
                kind: ItemKind::Shots,
                reason: format!("screenshots missing ({})", short.join(", ")),
            });
        }
    }

    // screenshots of changed pages, and images referenced but missing
    for (id, spec) in &state.shots {
        if stale_pages.contains_key(&spec.page) && page_guide(&sources, &spec.page, &primary).is_some() {
            plan.items.push(WorkItem {
                id: format!("shot:{id}"),
                kind: ItemKind::Shot,
                reason: format!("page {} changed; retake", spec.page),
            });
        }
    }
    let img = regex::Regex::new(r"!\[[^\]]*\]\((guide/img/([^/)]+)/([^)\s]+)\.(?:png|webp))\)").unwrap();
    for s in &sources {
        for c in img.captures_iter(&s.body) {
            let lang = &c[2];
            let id = &c[3];
            let file = cfg.out.join(&c[1]);
            if !file.is_file()
                && !file.with_extension("webp").is_file()
                && !plan.items.iter().any(|i| i.id == format!("shot:{id}"))
            {
                let known = state.shots.contains_key(id);
                plan.items.push(WorkItem {
                    id: format!("shot:{id}"),
                    kind: ItemKind::Shot,
                    reason: if known {
                        format!("image for {lang} is missing; retake")
                    } else {
                        format!(
                            "{} references it ({lang}) but there is no spec; create one with shoot",
                            s.path.display()
                        )
                    },
                });
            }
        }
    }

    // system document
    let server = crate::server::facts(cfg);
    let index = sources.iter().find(|s| s.path == Path::new("index.md"));
    let docs_text: String = sources
        .iter()
        .filter(|s| s.path == Path::new("index.md") || s.path.starts_with("system"))
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let setup_changed = changed_backend.iter().any(|p| {
        let s = p.to_string_lossy();
        ["composer.json", "package.json", ".env.example"].contains(&s.as_ref())
            || s.starts_with("config/")
            || s.starts_with("deploy/")
            || s.starts_with("docker")
    });
    let index_reason = if index.is_none() {
        Some("the start page is not written".to_string())
    } else {
        let missing = crate::server::undocumented(&server, &docs_text);
        if !missing.is_empty() {
            Some(format!("env keys not documented: {}", missing.join(", ")))
        } else if setup_changed {
            Some("server setup files changed (composer, config, deploy, env example)".into())
        } else {
            None
        }
    };
    if let Some(r) = index_reason {
        plan.items.push(WorkItem {
            id: "doc:index".into(),
            kind: ItemKind::Doc,
            reason: r,
        });
    }
    let routes_changed = changed_backend.iter().any(|p| {
        p.starts_with("routes") || p.starts_with("app/Http/Controllers") || p.starts_with("app/Http/Requests")
    });
    for (sec, what) in SYSTEM_SECTIONS {
        if !sources.iter().any(|s| s.path == Path::new(&format!("{sec}.md"))) {
            plan.items.push(WorkItem {
                id: format!("doc:{sec}"),
                kind: ItemKind::Doc,
                reason: format!("not written: {what}"),
            });
        } else if sec == "system/api" && routes_changed {
            plan.items.push(WorkItem {
                id: format!("doc:{sec}"),
                kind: ItemKind::Doc,
                reason: "routes or controllers changed".into(),
            });
        }
    }
    let modules: BTreeSet<String> = pages.iter().map(|p| p.module.clone()).collect();
    for m in modules {
        let rel = format!("system/modules/{}.md", source::slug(&m));
        let exists = sources.iter().any(|s| s.path == Path::new(&rel));
        let changed_pages: Vec<&String> = pages
            .iter()
            .filter(|p| p.module == m && stale_pages.get(&p.key).is_some_and(|r| r.contains("changed")))
            .map(|p| &p.key)
            .collect();
        let reason = if !exists {
            Some("not written".to_string())
        } else if !changed_pages.is_empty() {
            Some(format!(
                "pages changed: {}",
                changed_pages.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ))
        } else {
            None
        };
        if let Some(r) = reason {
            plan.items.push(WorkItem {
                id: format!("doc:system/modules/{}", source::slug(&m)),
                kind: ItemKind::Doc,
                reason: r,
            });
        }
    }

    // test targets
    let mut endpoints: BTreeMap<String, Endpoint> = BTreeMap::new();
    let mut units: Vec<(String, String)> = Vec::new(); // (target, source)
    let unit_files: Vec<PathBuf> = match (&changed, scope) {
        (_, Scope::All) | (None, _) => {
            let routes: Vec<serde_json::Value> = fb.all_routes()?.values().cloned().collect();
            for r in routes {
                let e = endpoint_from_route(&r, "");
                if e.action.contains('@') {
                    endpoints.insert(cases::endpoint_target(&e), e);
                }
            }
            // endpoints the pages call carry their validation rules
            for f in plan.facts.values() {
                for e in &f.endpoints {
                    endpoints.insert(cases::endpoint_target(e), e.clone());
                }
            }
            crate::issues::app_files(cfg)
                .into_iter()
                .filter(|p| unit_candidate(p))
                .collect()
        }
        (Some(_), Scope::Changes { .. } | Scope::Branch { .. }) => {
            for f in plan.facts.values() {
                for e in &f.endpoints {
                    endpoints.insert(cases::endpoint_target(e), e.clone());
                }
            }
            let php: Vec<String> = changed_backend
                .iter()
                .filter(|p| p.extension().is_some_and(|x| x == "php"))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            if !php.is_empty() {
                let a = fb.api().call(BACKEND, "find_usages", json!({"changed_files": php}))?;
                plan.stale_index |= a.stale;
                for c in a.data["changed"].as_array().into_iter().flatten() {
                    for r in c["routes_via_callers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.as_str())
                    {
                        let head = r.split(" (via").next().unwrap_or(r);
                        if let Some((methods, uri)) = head.split_once(' ') {
                            let method = methods.split('|').next().unwrap_or_default();
                            if let Some(e) = fb.endpoint(method, uri)? {
                                endpoints.insert(cases::endpoint_target(&e), e);
                            }
                        }
                    }
                }
                for p in &changed_backend {
                    if let Some(class) = class_of_file(p).filter(|_| p.starts_with("app/Http/Controllers")) {
                        let (routes, _) = fb.api().paged(
                            BACKEND,
                            "list_routes",
                            json!({"controller": class, "detail": "full"}),
                            "routes",
                        )?;
                        for r in routes {
                            let e0 = endpoint_from_route(&r, "");
                            if let Some(e) = fb.endpoint(&e0.method, &e0.uri)? {
                                endpoints.insert(cases::endpoint_target(&e), e);
                            }
                        }
                    }
                }
            }
            changed_backend.iter().filter(|p| unit_candidate(p)).cloned().collect()
        }
    };
    // public methods read straight from the PHP files (one codemap lookup per class cost minutes on big apps)
    let unit_total = unit_files.len();
    let unit_results: Vec<Result<Vec<(String, String)>, String>> = unit_files
        .iter()
        .enumerate()
        .map(|(n, f)| {
            progress.step(n + 1, unit_total, "reading classes for unit tests");
            let text = std::fs::read_to_string(cfg.backend.join(f)).unwrap_or_default();
            Ok(crate::testwrite::public_methods(&text)
                .map(|(class, ms)| ms.into_iter().map(|(m, src)| (format!("{class}::{m}"), src)).collect())
                .unwrap_or_default())
        })
        .collect();
    for r in unit_results {
        units.extend(r?);
    }
    plan.stale_index |= fb.stale;
    let endpoint_list: Vec<Endpoint> = endpoints.values().cloned().collect();
    // endpoints whose action was read (rules, messages); route-list endpoints carry none, so their
    // invalid:/boundary: cases are unknown, not absent
    let rules_read: BTreeSet<String> = endpoint_list
        .iter()
        .filter(|e| e.at.is_some())
        .map(cases::endpoint_target)
        .collect();
    let ledger = crate::ledger::scan(cfg, &endpoint_list);
    state.ledger = ledger.clone();
    let mut all_cases: Vec<Case> = Vec::new();
    for e in &endpoint_list {
        all_cases.extend(cases::for_endpoint(e));
    }
    for (target, src) in &units {
        all_cases.extend(cases::for_method(target, src));
    }
    let cases_total = all_cases.len();
    let mut cases_done = 0;
    for c in all_cases {
        if cases::covered_by(&ledger, &c.target, &c.case).is_some() {
            cases_done += 1;
        } else {
            plan.missing.entry(c.target.clone()).or_default().push(c);
        }
    }
    // doctest rows whose case or route is gone
    let derived = |c: &str| {
        ["ok", "guest", "forbidden", "not_found"].contains(&c)
            || ["invalid:", "boundary:", "throws:", "returns:"]
                .iter()
                .any(|p| c.starts_with(p))
    };
    let expected: BTreeMap<String, BTreeSet<String>> = {
        let mut m: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for e in &endpoint_list {
            m.entry(cases::endpoint_target(e))
                .or_default()
                .extend(cases::for_endpoint(e).into_iter().map(|c| c.case));
        }
        for (t, src) in &units {
            m.entry(t.clone())
                .or_default()
                .extend(cases::for_method(t, src).into_iter().map(|c| c.case));
        }
        m
    };
    let mut obsolete: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new(); // target → (route gone, cases)
    let doctest_rows: Vec<&crate::state::Covered> = ledger
        .iter()
        .filter(|c| c.source == crate::state::CoverSource::Doctest)
        .collect();
    let has_endpoint_rows = doctest_rows.iter().any(|c| {
        c.target
            .split_once(' ')
            .is_some_and(|(m, _)| m.chars().all(|x| x.is_ascii_uppercase()))
    });
    let routes: BTreeSet<(String, String)> = if has_endpoint_rows {
        fb.all_routes()?.keys().cloned().collect()
    } else {
        BTreeSet::new()
    };
    for c in doctest_rows {
        let route = c
            .target
            .split_once(' ')
            .filter(|(m, _)| m.chars().all(|x| x.is_ascii_uppercase()));
        if let Some((m, u)) = route
            && !routes.contains(&(m.to_string(), u.to_string()))
        {
            let e = obsolete.entry(c.target.clone()).or_default();
            e.0 = true;
            e.1.push(c.case.clone());
        } else if let Some(exp) = expected.get(&c.target)
            && derived(&c.case)
            && !exp.contains(&c.case)
            && (rules_read.contains(&c.target) || !(c.case.starts_with("invalid:") || c.case.starts_with("boundary:")))
            && (route.is_none() || rules_read.contains(&c.target) || !c.case.starts_with("throws:"))
        {
            obsolete.entry(c.target.clone()).or_default().1.push(c.case.clone());
        }
    }
    let mut targets: BTreeSet<String> = plan.missing.keys().cloned().collect();
    targets.extend(obsolete.keys().cloned());
    for target in &targets {
        let mut parts = Vec::new();
        if let Some((gone, cs)) = obsolete.get(target) {
            if *gone {
                parts.push(format!(
                    "route no longer exists: remove its rows ({}) with write_test remove",
                    cs.join(", ")
                ));
            } else {
                parts.push(format!("obsolete rows: {} (the rule, permission or branch is gone): remove or update them with write_test remove", cs.join(", ")));
            }
        }
        if let Some(cs) = plan.missing.get(target) {
            let list: Vec<&str> = cs.iter().map(|c| c.case.as_str()).collect();
            let note =
                if *scope == Scope::All && !list.iter().any(|c| c.starts_with("invalid:")) && target.contains(' ') {
                    " (validation cases are listed by `cases` once the action's rules are read)"
                } else {
                    ""
                };
            parts.push(format!("missing cases: {}{note}", list.join(", ")));
        }
        plan.items.push(WorkItem {
            id: format!("tests:{target}"),
            kind: ItemKind::Tests,
            reason: parts.join("; "),
        });
    }
    // whole-job coverage
    let documented = pages
        .iter()
        .filter(|p| {
            page_guide(&sources, &p.key, &primary).is_some()
                && state.pages.get(&p.key).is_some_and(|s| {
                    s.documented_fingerprint.as_deref() == Some(s.fingerprint.as_str()) && !s.fingerprint.is_empty()
                })
        })
        .count();
    let primaries: Vec<&Source> = sources
        .iter()
        .filter(|s| s.path.starts_with("guide") && s.fm.lang == primary)
        .collect();
    let others = cfg.languages.len().saturating_sub(1);
    let tr_total = primaries.len() * others;
    let tr_done = tr_total
        - plan
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Translation)
            .count()
            .min(tr_total);
    let img_refs: BTreeSet<String> = sources
        .iter()
        .flat_map(|s| img.captures_iter(&s.body).map(|c| c[1].to_string()).collect::<Vec<_>>())
        .collect();
    let img_done = img_refs.iter().filter(|r| cfg.out.join(r).is_file()).count();
    plan.coverage = Coverage {
        guides: Count::new(documented, pages.len()),
        translations: Count::new(tr_done, tr_total),
        screenshots: Count::new(img_done, img_refs.len()),
        test_cases: Count::new(cases_done, cases_total),
    };
    progress.step(1, 1, "plan ready");

    plan.items.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    plan.pages = pages;
    plan.endpoints = endpoint_list;
    plan.changed = changed;
    Ok(plan)
}

/// The portals each item belongs to: a page's guide, translation and screenshots its page's portal; a test target
/// the portals of the pages that call it, else the portal its URI names (`api/selfcare/…`). Items of no portal (the
/// system document, process guides, unit tests) are left out.
pub fn item_portals(plan: &Plan, state: &State, cfg: &Config) -> BTreeMap<String, Vec<String>> {
    let page_portal: BTreeMap<&str, &str> = plan
        .pages
        .iter()
        .filter(|p| !p.portal.is_empty())
        .map(|p| (p.key.as_str(), p.portal.as_str()))
        .collect();
    let mut callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in plan.facts.values() {
        if f.page.portal.is_empty() {
            continue;
        }
        for e in f.endpoints.iter().filter(|e| !e.possible) {
            callers
                .entry(cases::endpoint_target(e))
                .or_default()
                .insert(f.page.portal.clone());
        }
    }
    let portals: Vec<crate::config::Portal> = if cfg.guide.portals.is_empty() {
        plan.pages
            .iter()
            .map(|p| p.portal.clone())
            .filter(|p| !p.is_empty() && p != "public")
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|name| crate::config::Portal {
                pages: vec![format!("/{name}/*"), format!("{name}.*")],
                name,
                ..Default::default()
            })
            .collect()
    } else {
        cfg.guide.portals.clone()
    };
    let mut out = BTreeMap::new();
    for i in &plan.items {
        let mut parts = i.id.splitn(3, ':');
        let kind = parts.next().unwrap_or_default();
        let found: Vec<String> = match kind {
            "guide" | "translation" => {
                let key = parts.nth(1).unwrap_or_default();
                page_portal.get(key).map(|p| vec![p.to_string()]).unwrap_or_default()
            }
            "shots" => {
                let key = &i.id["shots:".len()..];
                page_portal.get(key).map(|p| vec![p.to_string()]).unwrap_or_default()
            }
            "shot" => {
                let id = &i.id["shot:".len()..];
                state
                    .shots
                    .get(id)
                    .and_then(|s| page_portal.get(s.page.as_str()))
                    .map(|p| vec![p.to_string()])
                    .unwrap_or_default()
            }
            "tests" => {
                let target = &i.id["tests:".len()..];
                match callers.get(target) {
                    Some(set) => set.iter().cloned().collect(),
                    None => target
                        .split_once(' ')
                        .filter(|(m, _)| m.chars().all(|c| c.is_ascii_uppercase()))
                        .map(|(_, uri)| {
                            let rest = uri.trim_start_matches('/');
                            let rest = rest.strip_prefix("api/").unwrap_or(rest);
                            let rest = rest
                                .split_once('/')
                                .filter(|(v, _)| v.len() <= 3 && v.starts_with('v'))
                                .map(|(_, r)| r)
                                .unwrap_or(rest);
                            crate::portals::portal_in(&portals, "", &format!("/{rest}"))
                        })
                        .filter(|p| !p.is_empty() && p != "public")
                        .map(|p| vec![p])
                        .unwrap_or_default(),
                }
            }
            _ => Vec::new(),
        };
        if !found.is_empty() {
            out.insert(i.id.clone(), found);
        }
    }
    out
}

/// Item kinds of a queue: `guides` (guides, translations, process guides, screenshots, system document) or `tests`.
pub fn queue_kinds(queue: &str) -> Result<Vec<String>, String> {
    match queue {
        "guides" | "guide" | "docs" => Ok(["guide", "translation", "process", "shots", "shot", "doc"]
            .map(String::from)
            .to_vec()),
        "tests" | "test" => Ok(vec!["tests".into()]),
        "" | "all" => Ok(Vec::new()),
        other => Err(format!("queue {other:?}: use guides or tests")),
    }
}

/// The kinds a claim or status asks for: the `kinds` given, narrowed to the queue's when both are given. "shot",
/// "shots" and "screenshots" all mean the screenshot items (`shots:<page>` and `shot:<id>`).
pub fn wanted_kinds(kinds: &[String], queue: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in kinds {
        let k = k.trim().trim_end_matches(':').to_lowercase();
        let names: Vec<String> = match k.as_str() {
            "shot" | "shots" | "screenshot" | "screenshots" => vec!["shots".into(), "shot".into()],
            "guides" => vec!["guide".into()],
            "translations" => vec!["translation".into()],
            "docs" => vec!["doc".into()],
            "test" => vec!["tests".into()],
            _ => vec![k],
        };
        for n in names {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    if queue.is_empty() {
        return out;
    }
    if out.is_empty() {
        return queue.to_vec();
    }
    let both: Vec<String> = out.into_iter().filter(|k| queue.contains(k)).collect();
    // kinds outside the queue: match nothing rather than everything
    if both.is_empty() { vec!["-".into()] } else { both }
}

/// Does an item pass a kinds and portals filter? An item of no portal passes any portal filter unless the filter
/// asks for `only` portal items.
pub fn item_wanted(
    id: &str,
    kinds: &[String],
    portals: &[String],
    item_portals: &BTreeMap<String, Vec<String>>,
) -> bool {
    let kind_ok = kinds.is_empty() || kinds.iter().any(|k| id.starts_with(&format!("{k}:")));
    let portal_ok = portals.is_empty()
        || match item_portals.get(id) {
            Some(ps) => ps.iter().any(|p| portals.contains(p)),
            None => {
                !id.starts_with("guide:")
                    && !id.starts_with("translation:")
                    && !id.starts_with("shot:")
                    && !id.starts_with("shots:")
            }
        };
    kind_ok && portal_ok
}

/// `125` → `2m 5s`.
pub fn duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m"),
    }
}

/// Redo: every page guide (and its screenshots) is listed again, even when it is up to date.
pub fn add_redo(plan: &mut Plan, cfg: &Config, state: &State) {
    let primary = cfg.primary_language();
    for p in &plan.pages {
        let id = format!("guide:{primary}:{}", p.key);
        if !plan.items.iter().any(|i| i.id == id) {
            plan.items.push(WorkItem {
                id,
                kind: ItemKind::Guide,
                reason: "redo requested: rewrite it and retake its screenshots".into(),
            });
        }
        for shot in state.pages.get(&p.key).map(|s| s.shots.clone()).unwrap_or_default() {
            let sid = format!("shot:{shot}");
            if !plan.items.iter().any(|i| i.id == sid) {
                plan.items.push(WorkItem {
                    id: sid,
                    kind: ItemKind::Shot,
                    reason: "redo requested".into(),
                });
            }
        }
    }
    plan.items.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub scope: String,
    pub progress: Coverage,
    pub counts: BTreeMap<String, usize>,
    pub items: Vec<WorkItem>,
    pub more: usize,
    pub stale_index: bool,
}

pub fn status(plan: &Plan, scope: &Scope, limit: usize) -> StatusReport {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for i in &plan.items {
        *counts.entry(format!("{:?}", i.kind).to_lowercase()).or_default() += 1;
    }
    StatusReport {
        scope: scope.name().into(),
        progress: plan.coverage.clone(),
        counts,
        items: plan.items.iter().take(limit).cloned().collect(),
        more: plan.items.len().saturating_sub(limit),
        stale_index: plan.stale_index,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codemap::FixtureCodemap;
    use crate::pages::tests::fixtures;
    use std::process::Command;

    pub fn sh(dir: &Path, args: &[&str]) {
        let o = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let p = e.path();
            let t = to.join(e.file_name());
            if p.is_dir() {
                copy_dir(&p, &t)
            } else {
                std::fs::copy(&p, &t).unwrap();
            }
        }
    }

    /// A git copy of the sample shop with .doctest.toml in its root.
    pub fn project() -> (tempfile::TempDir, Config) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("shop");
        copy_dir(&fixtures().join("shop"), &root);
        std::fs::write(
            root.join(".doctest.toml"),
            "[project]\nname=\"shop\"\nfrontends=[\"web\"]\nlanguages=[\"en\",\"id\"]\n[guide]\nmodules_from=\"web/src/config/menu.ts\"\nallow_bold=[\"Note\"]\nrequire_actions=false\nmin_screenshots=0\n",
        )
        .unwrap();
        sh(&root, &["init", "-q"]);
        sh(&root, &["config", "user.email", "t@t"]);
        sh(&root, &["config", "user.name", "t"]);
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-qm", "init"]);
        let cfg = Config::find(&root).unwrap();
        (d, cfg)
    }

    pub fn api() -> FixtureCodemap {
        FixtureCodemap::new(fixtures().join("codemap"))
    }

    fn ids(p: &Plan) -> Vec<String> {
        p.items.iter().map(|i| i.id.clone()).collect()
    }

    const GUIDE: &str = "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\norder: 1\n---\nClick **Save**.\n";

    fn write_source(cfg: &Config, rel: &str, text: &str) {
        let p = cfg.out.join("_source").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Marks the page as documented at its current fingerprint.
    fn document(cfg: &Config, state: &mut State) {
        write_source(cfg, "guide/en/orders/shop-orders-create.md", GUIDE);
        let p = plan(cfg, &mut api(), state, &Scope::All).unwrap();
        let fp = p.fingerprints["shop.orders.create"].clone();
        state
            .pages
            .get_mut("shop.orders.create")
            .unwrap()
            .documented_fingerprint = Some(fp);
    }

    #[test]
    fn first_run_plans_everything() {
        let (_d, cfg) = project();
        let mut st = State::default();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::Changes { since: None }).unwrap();
        let ids = ids(&p);
        for want in [
            "guide:en:shop.orders.create",
            "doc:index",
            "doc:system/architecture",
            "doc:system/modules/orders",
            "tests:POST api/orders/{order}",
        ] {
            assert!(ids.contains(&want.to_string()), "{want} not in {ids:?}");
        }
        assert!(
            !ids.iter().any(|i| i.starts_with("translation:")),
            "no translation before the primary guide exists"
        );
        assert!(
            p.items
                .iter()
                .find(|i| i.id == "guide:en:shop.orders.create")
                .unwrap()
                .reason
                .contains("first run")
        );
        assert!(
            st.pages["shop.orders.create"]
                .files
                .contains(&"src/pages/ShopOrderForm.vue".to_string())
        );
    }

    #[test]
    fn documented_page_not_planned_but_translation_is() {
        let (_d, cfg) = project();
        let mut st = State::default();
        document(&cfg, &mut st);
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        let ids = ids(&p);
        assert!(!ids.contains(&"guide:en:shop.orders.create".to_string()), "{ids:?}");
        assert!(
            ids.contains(&"translation:id:shop.orders.create".to_string()),
            "{ids:?}"
        );
    }

    #[test]
    fn changed_label_marks_guide_stale_in_changes_scope() {
        let (_d, cfg) = project();
        let mut st = State::default();
        document(&cfg, &mut st);
        st.last_commit = commits(&cfg).unwrap();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::Changes { since: None }).unwrap();
        assert!(
            !ids(&p).iter().any(|i| i.starts_with("guide:")),
            "nothing changed: {:?}",
            ids(&p)
        );
        let en = cfg.frontends[0].join("src/i18n/locales/en.json");
        let t = std::fs::read_to_string(&en)
            .unwrap()
            .replace("\"Tenant\"", "\"Customer\"");
        std::fs::write(&en, t).unwrap();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::Changes { since: None }).unwrap();
        let g = p
            .items
            .iter()
            .find(|i| i.id == "guide:en:shop.orders.create")
            .unwrap_or_else(|| panic!("{:?}", ids(&p)));
        assert!(g.reason.contains("changed"), "{}", g.reason);
    }

    #[test]
    fn translation_stale_when_primary_body_changes() {
        let (_d, cfg) = project();
        let mut st = State::default();
        document(&cfg, &mut st);
        let h = crate::source::body_hash("Click **Save**.\n");
        write_source(
            &cfg,
            "guide/id/orders/shop-orders-create.md",
            &format!(
                "---\ntitle: Pesanan Baru\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\ntranslated_from: {h}\n---\nKlik **Simpan**.\n"
            ),
        );
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        assert!(
            !ids(&p).contains(&"translation:id:shop.orders.create".to_string()),
            "{:?}",
            ids(&p)
        );
        write_source(
            &cfg,
            "guide/en/orders/shop-orders-create.md",
            &GUIDE.replace("Click **Save**.", "Click **Save** to keep it."),
        );
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        let t = p
            .items
            .iter()
            .find(|i| i.id == "translation:id:shop.orders.create")
            .unwrap_or_else(|| panic!("{:?}", ids(&p)));
        assert!(t.reason.contains("en guide changed"), "{}", t.reason);
    }

    #[test]
    fn written_guides_short_of_screenshots_get_a_shots_item() {
        let (_d, mut cfg) = project();
        cfg.guide.min_screenshots = None;
        let mut st = State::default();
        document(&cfg, &mut st);
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        let item = p
            .items
            .iter()
            .find(|i| i.id == "shots:shop.orders.create")
            .unwrap_or_else(|| panic!("{:?}", ids(&p)));
        assert_eq!(item.kind, ItemKind::Shots);
        assert!(item.reason.contains("en 0/"), "{}", item.reason);
        assert!(queue_kinds("guides").unwrap().contains(&"shots".to_string()));
        // adding a screenshot to the guide does not make its translations stale
        let h = crate::source::body_hash("Click **Save**.\n");
        write_source(
            &cfg,
            "guide/id/orders/shop-orders-create.md",
            &format!(
                "---\ntitle: Pesanan Baru\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\ntranslated_from: {h}\n---\nKlik **Simpan**.\n"
            ),
        );
        write_source(
            &cfg,
            "guide/en/orders/shop-orders-create.md",
            &GUIDE.replace(
                "Click **Save**.",
                "Click **Save**.\n\n![Orders](guide/img/en/orders/shop-orders-create-1.webp)",
            ),
        );
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        assert!(
            !ids(&p).contains(&"translation:id:shop.orders.create".to_string()),
            "{:?}",
            ids(&p)
        );
    }

    #[test]
    fn kinds_narrow_the_queue() {
        let guides = queue_kinds("guides").unwrap();
        let k = wanted_kinds(&["translation".into()], &guides);
        assert_eq!(k, vec!["translation".to_string()]);
        let ip = BTreeMap::new();
        assert!(item_wanted("translation:id:x", &k, &[], &ip));
        assert!(!item_wanted("guide:en:x", &k, &[], &ip) && !item_wanted("doc:system/modules/a", &k, &[], &ip));
        let k = wanted_kinds(&["shot".into()], &guides);
        assert!(item_wanted("shots:x", &k, &[], &ip) && item_wanted("shot:m/x-1", &k, &[], &ip));
        assert!(!item_wanted("guide:en:x", &k, &[], &ip));
        let k = wanted_kinds(&["guide".into(), "process".into(), "doc".into()], &guides);
        assert!(!item_wanted("translation:id:x", &k, &[], &ip));
        let k = wanted_kinds(&["tests".into()], &guides);
        assert!(!item_wanted("guide:en:x", &k, &[], &ip) && !item_wanted("tests:x", &k, &[], &ip));
        assert_eq!(wanted_kinds(&[], &guides), guides);
    }

    #[test]
    fn covered_cases_not_planned() {
        let (_d, cfg) = project();
        let mut st = State::default();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        let missing: Vec<&str> = p.missing["POST api/orders/{order}"]
            .iter()
            .map(|c| c.case.as_str())
            .collect();
        for covered in ["ok", "guest", "invalid:email:required", "invalid:qty:required"] {
            assert!(
                !missing.contains(&covered),
                "{covered} is covered but planned: {missing:?}"
            );
        }
        for want in ["forbidden", "not_found", "invalid:email:email", "boundary:qty:min"] {
            assert!(missing.contains(&want), "{want} not in {missing:?}");
        }
    }

    #[test]
    fn changed_controller_plans_its_endpoint_tests() {
        let (_d, cfg) = project();
        let mut st = State::default();
        document(&cfg, &mut st);
        st.last_commit = commits(&cfg).unwrap();
        let f = cfg.backend.join("app/Http/Controllers/OrderController.php");
        let t = std::fs::read_to_string(&f)
            .unwrap()
            .replace("Order saved.", "Order stored.");
        std::fs::write(&f, t).unwrap();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::Changes { since: None }).unwrap();
        assert!(
            ids(&p).contains(&"tests:POST api/orders/{order}".to_string()),
            "{:?}",
            ids(&p)
        );
        assert!(
            ids(&p).contains(&"guide:en:shop.orders.create".to_string()),
            "the page's endpoint messages changed: {:?}",
            ids(&p)
        );
    }

    #[test]
    fn plan_reports_progress_and_coverage() {
        let (_d, cfg) = project();
        let mut st = State::default();
        let rec = crate::progress::Recorder::default();
        let p = plan_with(&cfg, &mut api(), &mut st, &Scope::All, &rec).unwrap();
        let steps = rec.steps();
        assert!(!steps.is_empty());
        let last = steps.last().unwrap();
        assert_eq!(last.0, last.1, "{steps:?}");
        assert_eq!((p.coverage.guides.done, p.coverage.guides.total), (0, 1));
        assert!(p.coverage.test_cases.total > p.coverage.test_cases.done && p.coverage.test_cases.done >= 4);
        document(&cfg, &mut st);
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        assert_eq!(
            (
                p.coverage.guides.done,
                p.coverage.guides.total,
                p.coverage.guides.percent
            ),
            (1, 1, 100)
        );
        assert_eq!((p.coverage.translations.done, p.coverage.translations.total), (0, 1));
    }

    #[test]
    fn branch_scope_plans_what_the_branch_changed() {
        let (_d, cfg) = project();
        let mut st = State::default();
        document(&cfg, &mut st);
        sh(&cfg.backend, &["branch", "-M", "master"]);
        sh(&cfg.backend, &["switch", "-qc", "feat/label"]);
        let en = cfg.frontends[0].join("src/i18n/locales/en.json");
        let t = std::fs::read_to_string(&en)
            .unwrap()
            .replace("\"Tenant\"", "\"Customer\"");
        std::fs::write(&en, t).unwrap();
        sh(&cfg.backend, &["commit", "-qam", "rename"]);
        let p = plan(&cfg, &mut api(), &mut st, &Scope::Branch { base: None }).unwrap();
        assert!(
            ids(&p).contains(&"guide:en:shop.orders.create".to_string()),
            "{:?}",
            ids(&p)
        );
        assert!(
            p.changed
                .as_ref()
                .unwrap()
                .values()
                .flatten()
                .any(|f| f.ends_with("en.json"))
        );
    }

    #[test]
    fn excluded_pages_are_not_planned() {
        let (_d, cfg) = project();
        let mut cfg = cfg;
        cfg.guide.exclude = vec!["shop.orders.*".into()];
        let mut st = State::default();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        assert!(!ids(&p).iter().any(|i| i.starts_with("guide:")), "{:?}", ids(&p));
        assert!(page_matches(
            "admin.rems.data.lease-out",
            "/admin/rems/data/lease-out",
            &["admin.*".into()]
        ));
        assert!(page_matches("x", "/admin/rems", &["/admin/*".into()]));
        assert!(!page_matches("selfcare.home", "/selfcare", &["admin.*".into()]));
    }

    #[test]
    fn parallel_plan_matches_serial() {
        let (_d, cfg) = project();
        let mut st1 = State::default();
        let serial = plan(&cfg, &mut api(), &mut st1, &Scope::All).unwrap();
        let mut cfg4 = cfg.clone();
        cfg4.workers = 4;
        let mut st2 = State::default();
        let factory = || -> Box<dyn CodemapApi + Send> { Box::new(api()) };
        let rec = crate::progress::Recorder::default();
        cfg4.parallel_reads = true;
        let par = plan_parallel(&cfg4, &mut api(), &mut st2, &Scope::All, &rec, Some(&factory)).unwrap();
        assert_eq!(ids(&serial), ids(&par));
        assert_eq!(st1.pages, st2.pages);
    }

    fn doctest_rows(cfg: &Config, target: &str, cases: &[&str]) {
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let rows = cases
            .iter()
            .map(|c| crate::testwrite::Row {
                case: c.to_string(),
                args_php: "1".into(),
                bug: None,
            })
            .collect();
        let m = format!("test_dt_{}", crate::source::slug(target).replace('-', "_"));
        let t = crate::testwrite::NewTests {
            method: m.clone(),
            method_php: Some(format!("public function {m}(int $x): void {{}}")),
            rows,
        };
        crate::testwrite::insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            target,
            &t,
        )
        .unwrap();
    }

    #[test]
    fn obsolete_rows_are_flagged() {
        let (_d, cfg) = project();
        doctest_rows(&cfg, "POST api/orders/{order}", &["forbidden", "invalid:note:required"]);
        doctest_rows(&cfg, "GET api/gone", &["ok"]);
        let mut st = State::default();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        let orders = p
            .items
            .iter()
            .find(|i| i.id == "tests:POST api/orders/{order}")
            .unwrap();
        assert!(
            orders.reason.contains("obsolete rows: invalid:note:required"),
            "{}",
            orders.reason
        );
        assert!(!orders.reason.contains("obsolete rows: forbidden"));
        let gone = p
            .items
            .iter()
            .find(|i| i.id == "tests:GET api/gone")
            .unwrap_or_else(|| panic!("{:?}", ids(&p)));
        assert!(gone.reason.contains("route no longer exists"), "{}", gone.reason);
    }

    #[test]
    fn rules_never_read_do_not_make_rows_obsolete() {
        let (_d, cfg) = project();
        // DELETE api/everything comes only from list_routes (no page calls it), so its rules were never read
        doctest_rows(&cfg, "DELETE api/everything", &["invalid:id:required"]);
        let mut st = State::default();
        let p = plan(&cfg, &mut api(), &mut st, &Scope::All).unwrap();
        let item = p.items.iter().find(|i| i.id == "tests:DELETE api/everything");
        assert!(item.is_none_or(|i| !i.reason.contains("obsolete")), "{item:?}");
    }

    #[test]
    fn route_changes_flag_the_api_document() {
        let (_d, cfg) = project();
        let mut st = State::default();
        let p = cfg.out.join("_source/system/api.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "---\ntitle: API\n---\nx\n").unwrap();
        st.last_commit = commits(&cfg).unwrap();
        let f = cfg.backend.join("app/Http/Controllers/OrderController.php");
        let t = std::fs::read_to_string(&f)
            .unwrap()
            .replace("Order saved.", "Order stored.");
        std::fs::write(&f, t).unwrap();
        let plan = plan(&cfg, &mut api(), &mut st, &Scope::Changes { since: None }).unwrap();
        let api_item = plan
            .items
            .iter()
            .find(|i| i.id == "doc:system/api")
            .unwrap_or_else(|| panic!("{:?}", ids(&plan)));
        assert!(
            api_item.reason.contains("routes or controllers changed"),
            "{}",
            api_item.reason
        );
    }

    #[test]
    fn stale_codemap_reported() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("list_pages.json"),
            r#"{"data":{"pages":[]},"meta":{"stale":true}}"#,
        )
        .unwrap();
        let (_p, cfg) = project();
        let mut st = State::default();
        let p = plan(
            &cfg,
            &mut FixtureCodemap::new(d.path().to_path_buf()),
            &mut st,
            &Scope::All,
        )
        .unwrap();
        assert!(p.stale_index);
    }
}
