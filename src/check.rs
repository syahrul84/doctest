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

pub fn run(
    cfg: &Config,
    api: &mut dyn CodemapApi,
    state: &mut State,
    progress: &dyn crate::progress::Progress,
) -> Result<CheckReport, String> {
    let mut r = CheckReport::default();
    let sources = source::sources(cfg)?;
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    {
        let mut fb = FactBuilder::new(api, cfg, &labels);
        let pages = fb.pages()?;
        for (n, s) in sources.iter().enumerate() {
            progress.step(n + 1, sources.len(), "checking guides against their pages");
            let mut facts = None;
            if s.fm.kind == Kind::Page {
                let key = s.fm.routes.first().cloned().unwrap_or_default();
                match pages.iter().find(|p| p.key == key) {
                    Some(page) => {
                        let f = fb.facts(page)?;
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
