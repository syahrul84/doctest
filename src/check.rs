//! One check over the document folder and doctest's tests, for CI and pre-commit: guides against their pages,
//! links and images, undocumented env keys, stale pages, failing doctest tests (errors); known-bug tests listed
//! apart; code problems listed as points to fix.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::codemap::CodemapApi;
use crate::config::Config;
use crate::i18n::Labels;
use crate::issues::{Issue, IssueScanner, frontend_files, frontend_issues};
use crate::pages::{FactBuilder, fingerprint};
use crate::source::{self, Kind};
use crate::state::State;
use crate::validate::{Level, check_source};

#[derive(Debug, Default, Serialize)]
pub struct CheckReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub bugs: Vec<String>,
    pub points: Vec<Issue>,
}

impl CheckReport {
    pub fn passed(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Which guides a check looks at: all, the ones git sees changed, or the ones named (route keys or source paths, `*`
/// allowed). A narrowed check skips the project-wide parts (env keys, unused images, the code scan).
#[derive(Debug, Default, Clone)]
pub struct Only {
    pub changed: bool,
    pub names: Vec<String>,
}

impl Only {
    pub fn all(&self) -> bool {
        !self.changed && self.names.is_empty()
    }
}

/// Guide sources git sees as changed or new under the document folder.
fn git_changed_sources(cfg: &Config) -> BTreeSet<std::path::PathBuf> {
    let root = source::source_root(cfg);
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all", "--"])
        .arg(&root)
        .current_dir(&cfg.root)
        .output();
    let Ok(out) = out else { return BTreeSet::new() };
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cfg.root)
        .output()
        .ok()
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_else(|| cfg.root.clone());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.get(3..))
        .map(|p| p.rsplit(" -> ").next().unwrap_or(p).trim_matches('"'))
        .filter_map(|p| top.join(p).strip_prefix(&root).ok().map(|r| r.to_path_buf()))
        .collect()
}

#[cfg(test)]
pub fn run(
    cfg: &Config,
    api: &mut dyn CodemapApi,
    state: &mut State,
    progress: &dyn crate::progress::Progress,
) -> Result<CheckReport, String> {
    run_only(cfg, api, state, progress, &Only::default())
}

pub fn run_only(
    cfg: &Config,
    api: &mut dyn CodemapApi,
    state: &mut State,
    progress: &dyn crate::progress::Progress,
    only: &Only,
) -> Result<CheckReport, String> {
    let mut r = CheckReport::default();
    let sources = source::sources(cfg)?;
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let changed = if only.changed {
        git_changed_sources(cfg)
    } else {
        BTreeSet::new()
    };
    let wanted = |s: &source::Source| {
        if only.all() {
            return true;
        }
        let key = s.fm.routes.first().cloned().unwrap_or_default();
        (only.changed && changed.contains(&s.path))
            || only.names.iter().any(|n| {
                let path = s.path.to_string_lossy();
                crate::work::page_matches(&key, &path, std::slice::from_ref(n))
                    || path == n.trim_start_matches("_source/")
            })
    };
    let picked: Vec<&source::Source> = sources.iter().filter(|s| wanted(s)).collect();
    {
        let mut fb = FactBuilder::new(api, cfg, &labels);
        let pages = fb.pages()?;
        // facts once per page (every language's guide uses the same), kept between runs while the page's code is unchanged
        let mut cache = crate::factcache::FactCache::load(cfg, labels.files_hash());
        let mut by_page: std::collections::HashMap<String, crate::pages::PageFacts> = std::collections::HashMap::new();
        let started = std::time::Instant::now();
        for (n, s) in picked.iter().enumerate() {
            let left = if n >= 5 {
                let per = started.elapsed().as_secs_f64() / n as f64;
                let secs = (per * (picked.len() - n) as f64) as u64;
                format!(" (about {} left)", crate::work::duration(secs))
            } else {
                String::new()
            };
            progress.step(
                n + 1,
                picked.len(),
                &format!("checking guides against their pages{left}"),
            );
            let mut facts = None;
            if s.fm.kind == Kind::Page {
                let key = s.fm.routes.first().cloned().unwrap_or_default();
                match pages.iter().find(|p| p.key == key) {
                    Some(page) => {
                        let f = match by_page.get(&key) {
                            Some(f) => f.clone(),
                            None => {
                                let f = match cache.get(cfg, &key) {
                                    Some(f) => f,
                                    None => {
                                        let f = fb.facts(page)?;
                                        cache.put(cfg, &f);
                                        f
                                    }
                                };
                                by_page.insert(key.clone(), f.clone());
                                f
                            }
                        };
                        if s.fm.lang == cfg.primary_language() {
                            let fp = fingerprint(&f, labels.files_hash());
                            if state.pages.get(&key).and_then(|p| p.documented_fingerprint.as_deref())
                                != Some(fp.as_str())
                            {
                                r.warnings.push(format!(
                                    "{}: page {key} changed since this guide was written",
                                    s.path.display()
                                ));
                            }
                        }
                        facts = Some(f);
                    }
                    None => r
                        .errors
                        .push(format!("{}: page {key} no longer exists", s.path.display())),
                }
            }
            for p in check_source(s, facts.as_ref(), cfg, &sources) {
                let msg = format!("{}: {}", s.path.display(), p.message);
                match p.level {
                    Level::Error => r.errors.push(msg),
                    Level::Warning => r.warnings.push(msg),
                }
            }
        }
        cache.save();
    }
    if !only.all() {
        // a narrowed check: only the guides asked for
        r.warnings.push(format!(
            "checked {} of {} guide sources (narrowed); run check without a filter for the whole project",
            picked.len(),
            sources.len()
        ));
        return Ok(r);
    }
    let docs: String = sources
        .iter()
        .filter(|s| s.path == Path::new("index.md") || s.path.starts_with("system"))
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !sources.iter().any(|s| s.path == Path::new("index.md")) {
        r.errors
            .push("the start page (index) is not written; doctest lists it as doc:index".into());
    } else {
        let missing = crate::server::undocumented(&crate::server::facts(cfg), &docs);
        if !missing.is_empty() {
            r.errors.push(format!(
                "env keys the code reads but the docs do not mention: {}",
                missing.join(", ")
            ));
        }
    }
    let img_ref = regex::Regex::new(r"\]\((guide/img/[^)\s]+)\)").unwrap();
    let referenced: BTreeSet<String> = sources
        .iter()
        .flat_map(|s| {
            img_ref
                .captures_iter(&s.body)
                .map(|c| c[1].to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let mut imgs = Vec::new();
    collect(&cfg.out, &cfg.out.join("guide/img"), &mut imgs);
    for i in imgs {
        if !referenced.contains(&i) {
            r.warnings.push(format!("{i} is not used by any guide"));
        }
    }
    for f in &state.failing {
        r.errors.push(format!("doctest test failing: {f}"));
    }
    for c in crate::ledger::scan(cfg, &[]) {
        if let Some(b) = &c.bug {
            r.bugs.push(format!("{} {} ({}): {b}", c.target, c.case, c.test));
        }
    }
    let mut scanner = IssueScanner::new(api, cfg);
    r.points = scanner.scan_files_with(&crate::issues::app_files(cfg), progress)?;
    for fe in &cfg.frontends {
        r.points.extend(frontend_issues(fe, &frontend_files(fe)));
    }
    Ok(r)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tests::{api, project};

    fn write_source(cfg: &Config, rel: &str, text: &str) {
        let p = cfg.out.join("_source").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn check_fails_on_unknown_ui_name_and_undocumented_env() {
        let (_d, cfg) = project();
        write_source(
            &cfg,
            "guide/en/orders/shop-orders-create.md",
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Submit**.\n",
        );
        write_source(&cfg, "index.md", "---\ntitle: Start\n---\nSet APP_NAME.\n");
        let mut st = State::default();
        let r = run(&cfg, &mut api(), &mut st, &crate::progress::Silent).unwrap();
        assert!(
            r.errors.iter().any(|e| e.contains("unknown UI name \"Submit\"")),
            "{:#?}",
            r.errors
        );
        assert!(r.errors.iter().any(|e| e.contains("MAIL_HOST")), "{:#?}", r.errors);
        assert!(!r.passed());
    }

    #[test]
    fn check_lists_bug_tests_without_failing() {
        let (_d, cfg) = project();
        write_source(
            &cfg,
            "index.md",
            "---\ntitle: Start\n---\nAPP_NAME APP_URL DB_HOST QUEUE_CONNECTION MAIL_HOST BROADCAST_CONNECTION\n",
        );
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let bug = crate::testwrite::Row {
            case: "forbidden".into(),
            args_php: "403".into(),
            bug: Some("anyone can order".into()),
        };
        let t = crate::testwrite::NewTests {
            method: "test_store".into(),
            method_php: Some("public function test_store(int $s): void {}".into()),
            rows: vec![bug],
        };
        crate::testwrite::insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            "POST api/orders/{order}",
            &t,
        )
        .unwrap();
        let mut st = State::default();
        let r = run(&cfg, &mut api(), &mut st, &crate::progress::Silent).unwrap();
        assert!(r.bugs.iter().any(|b| b.contains("anyone can order")), "{:#?}", r.bugs);
        assert!(r.errors.is_empty(), "{:#?}", r.errors);
        assert!(r.passed());
    }

    #[test]
    fn check_reports_failing_doctest_tests_and_code_points() {
        let (_d, cfg) = project();
        write_source(
            &cfg,
            "index.md",
            "---\ntitle: Start\n---\nAPP_NAME APP_URL DB_HOST QUEUE_CONNECTION MAIL_HOST BROADCAST_CONNECTION\n",
        );
        let mut st = State::default();
        st.failing
            .push("OrderControllerTest::test_store with data set \"guest\"".into());
        let r = run(&cfg, &mut api(), &mut st, &crate::progress::Silent).unwrap();
        assert!(r.errors.iter().any(|e| e.contains("test_store")), "{:#?}", r.errors);
        assert!(
            r.points.iter().any(|p| p.kind == crate::issues::IssueKind::NPlusOne),
            "{:#?}",
            r.points
        );
    }
}
