//! Validates the current changes: runs the tests the change affects (codemap) and doctest's tests for the changed
//! targets, lists code problems on the changed lines, and what is left to document or test. Passes when no test
//! fails and no changed line has a problem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;
use serde_json::json;

use crate::codemap::{BACKEND, CodemapApi};
use crate::config::Config;
use crate::issues::{Issue, IssueKind, IssueScanner, failing_tests, frontend_issues};
use crate::state::{CoverSource, State};
use crate::work::{Scope, WorkItem};

#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub pass: bool,
    /// Points that make the change fail: failing tests, problems on changed lines.
    pub errors: Vec<Issue>,
    /// What is left to document or test for the change.
    pub todo: Vec<WorkItem>,
    pub tests_run: Vec<String>,
    pub test_output: String,
}

pub fn run(
    cfg: &Config,
    api: &mut dyn CodemapApi,
    state: &mut State,
    since: Option<BTreeMap<String, String>>,
    progress: &dyn crate::progress::Progress,
) -> Result<VerifyReport, String> {
    let since = match since {
        Some(s) => s,
        None if !state.last_commit.is_empty() => state.last_commit.clone(),
        None => crate::work::commits(cfg)?,
    };
    let changed = crate::work::changed_files(cfg, &since)?;
    let mut r = VerifyReport::default();
    if changed.values().all(Vec::is_empty) {
        r.pass = true;
        return Ok(r);
    }
    let plan = crate::work::plan_with(
        cfg,
        api,
        state,
        &Scope::Changes {
            since: Some(since.clone()),
        },
        progress,
    )?;
    r.todo = plan.items.clone();

    let backend_name = Config::repo_name(&cfg.backend);
    let changed_backend: Vec<String> = changed
        .get(&backend_name)
        .into_iter()
        .flatten()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let php: Vec<String> = changed_backend
        .iter()
        .filter(|p| p.ends_with(".php"))
        .cloned()
        .collect();

    // tests to run: what codemap says the change affects, plus doctest's tests for the changed targets
    let mut files: BTreeSet<String> = BTreeSet::new();
    let app_php: Vec<String> = php.iter().filter(|p| !p.starts_with("tests/")).cloned().collect();
    if !app_php.is_empty() {
        let a = api.call(BACKEND, "find_usages", json!({"changed_files": app_php}))?;
        for t in a.data["tests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str())
        {
            files.insert(t.to_string());
        }
    }
    for p in &php {
        if p.starts_with("tests/") && cfg.backend.join(p).is_file() {
            files.insert(p.clone());
        }
    }
    let targets: BTreeSet<String> = plan.endpoints.iter().map(crate::cases::endpoint_target).collect();
    for c in &state.ledger {
        if c.source == CoverSource::Doctest && targets.contains(&c.target) {
            files.insert(c.file.to_string_lossy().replace('\\', "/"));
        }
    }
    r.tests_run = files.iter().cloned().collect();
    if !r.tests_run.is_empty() {
        progress.step(0, 1, &format!("running {} affected test file(s)", r.tests_run.len()));
        let res = {
            let (_lock, slot) = State::test_slot(cfg)?;
            crate::testwrite::run_files(cfg, &r.tests_run, slot)?
        };
        r.test_output = crate::testwrite::tail(&res.output, 80);
        r.errors.extend(failing_tests(&res));
        progress.step(1, 1, &format!("running {} affected test file(s)", r.tests_run.len()));
    }

    // problems on changed lines
    let backend_lines = crate::git::changed_lines(
        &cfg.backend,
        since.get(&backend_name).map(String::as_str).unwrap_or("HEAD"),
    )?;
    let scan: Vec<PathBuf> = php
        .iter()
        .filter(|p| p.starts_with("app/"))
        .map(PathBuf::from)
        .collect();
    if !scan.is_empty() {
        let mut scanner = IssueScanner::new(api, cfg);
        for i in scanner.scan_files(&scan)? {
            let on_changed = backend_lines
                .get(&PathBuf::from(&i.file))
                .is_some_and(|l| l.contains(&i.line));
            let changed_migration = i.kind == IssueKind::MissingIndex && changed_backend.contains(&i.file);
            if on_changed || changed_migration {
                r.errors.push(i);
            }
        }
    }
    for fe in &cfg.frontends {
        let name = Config::repo_name(fe);
        let Some(Some(commit)) = Some(since.get(&name)) else {
            continue;
        };
        let files: Vec<PathBuf> = changed
            .get(&name)
            .into_iter()
            .flatten()
            .filter(|p| {
                p.starts_with(Config::src_rel(fe))
                    && !p.starts_with("node_modules")
                    && p.extension().is_some_and(|x| x == "vue" || x == "ts" || x == "js")
            })
            .cloned()
            .collect();
        if files.is_empty() {
            continue;
        }
        let lines = crate::git::changed_lines(fe, commit)?;
        for i in frontend_issues(fe, &files) {
            if lines.get(&PathBuf::from(&i.file)).is_some_and(|l| l.contains(&i.line)) {
                r.errors.push(i);
            }
        }
    }
    r.pass = r.errors.is_empty();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tests::{api, project, sh};

    fn with_fake_runner(cfg: &Config, output: &str) -> Config {
        std::fs::write(cfg.backend.join("fake-test-output.txt"), output).unwrap();
        let mut c = cfg.clone();
        c.tests.run = "cat fake-test-output.txt # {filter}".into();
        c
    }

    fn baseline(cfg: &Config, st: &mut State) {
        st.last_commit = crate::work::commits(cfg).unwrap();
    }

    #[test]
    fn problem_on_a_changed_line_fails_verify() {
        let (_d, cfg) = project();
        let cfg = with_fake_runner(&cfg, "  ✓ order needs an email\n\n  Tests:  1 passed\n");
        let mut st = State::default();
        baseline(&cfg, &mut st);
        let f = cfg.backend.join("app/Http/Controllers/OrderController.php");
        let t = std::fs::read_to_string(&f).unwrap().replace(
            "        $row = ['status' => 'new'];",
            "        $row = ['status' => 'new'];\n        sleep(1);",
        );
        std::fs::write(&f, t).unwrap();
        let r = run(&cfg, &mut api(), &mut st, None, &crate::progress::Silent).unwrap();
        assert!(!r.pass);
        let p = r
            .errors
            .iter()
            .find(|i| i.kind == crate::issues::IssueKind::SleepInRequest)
            .unwrap_or_else(|| panic!("{:#?}", r.errors));
        assert_eq!(
            (p.file.as_str(), p.line),
            ("app/Http/Controllers/OrderController.php", 16)
        );
        assert_eq!(
            r.tests_run,
            vec!["tests/Feature/Http/Controllers/OrderControllerTest.php"]
        );
    }

    #[test]
    fn old_problems_on_unchanged_lines_do_not_count() {
        let (_d, cfg) = project();
        let cfg = with_fake_runner(&cfg, "  ✓ order needs an email\n\n  Tests:  1 passed\n");
        let mut st = State::default();
        baseline(&cfg, &mut st);
        // Slow.php is full of problems but unchanged; touch only a comment in OrderController
        let f = cfg.backend.join("app/Http/Controllers/OrderController.php");
        let t = std::fs::read_to_string(&f)
            .unwrap()
            .replace("class OrderController", "/** Orders. */\nclass OrderController");
        std::fs::write(&f, t).unwrap();
        let r = run(&cfg, &mut api(), &mut st, None, &crate::progress::Silent).unwrap();
        assert!(r.errors.is_empty(), "{:#?}", r.errors);
        assert!(r.pass);
        assert!(
            r.todo.iter().any(|i| i.id == "tests:POST api/orders/{order}"),
            "{:#?}",
            r.todo
        );
    }

    #[test]
    fn failing_affected_test_fails_verify() {
        let (_d, cfg) = project();
        let cfg = with_fake_runner(
            &cfg,
            "  ⨯ order needs an email\n   FAILED  Tests\\Feature\\Http\\Controllers\\OrderControllerTest > order needs an email\n  Expected response status code [422] but received 200.\n\n  at tests/Feature/Http/Controllers/OrderControllerTest.php:13\n\n  Tests:  1 failed\n",
        );
        let mut st = State::default();
        baseline(&cfg, &mut st);
        let f = cfg.backend.join("app/Http/Controllers/OrderController.php");
        let t = std::fs::read_to_string(&f)
            .unwrap()
            .replace("'required|email|max:255'", "'nullable|email|max:255'");
        std::fs::write(&f, t).unwrap();
        let r = run(&cfg, &mut api(), &mut st, None, &crate::progress::Silent).unwrap();
        assert!(!r.pass);
        let p = r
            .errors
            .iter()
            .find(|i| i.kind == crate::issues::IssueKind::FailingTest)
            .unwrap_or_else(|| panic!("{:#?}", r.errors));
        assert_eq!(
            (p.file.as_str(), p.line),
            ("tests/Feature/Http/Controllers/OrderControllerTest.php", 13)
        );
    }

    #[test]
    fn no_changes_passes_with_nothing_to_run() {
        let (_d, cfg) = project();
        let mut st = State::default();
        baseline(&cfg, &mut st);
        sh(&cfg.backend, &["status"]);
        let r = run(&cfg, &mut api(), &mut st, None, &crate::progress::Silent).unwrap();
        assert!(r.pass && r.errors.is_empty() && r.tests_run.is_empty(), "{r:#?}");
    }
}
