//! MCP over stdio: the tools Claude uses to keep a project's guides, system document and tests current.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::cases;
use crate::codemap::{BACKEND, CodemapApi, LiveCodemap};
use crate::config::Config;
use crate::i18n::Labels;
use crate::issues::{IssueScanner, frontend_files, frontend_issues};
use crate::pages::{FactBuilder, fingerprint};
use crate::shots::{Shooter, ShotSpec};
use crate::source::{self, Kind, Source};
use crate::state::{CoverSource, RunRecord, State};
use crate::testwrite::{self, NewTests};
use crate::work::{self, Scope};

pub const MCP_INSTRUCTIONS: &str = "Every doctest reply ends with a summary: how long the step took, what it found (errors, failures, counts) and what to do next — follow its next line.\n\ndoctest keeps a Laravel + Vue project's user guide (per page, per language), its system document (static HTML in document/, starting at index.html with server setup from scratch) and its PHPUnit tests current. You write; doctest finds what is missing or stale, gives you the facts, checks what you write, takes screenshots, runs tests and builds the HTML.\n\nWorkflow:\n1. status (scope changes = since the last finish; branch = what this branch changed since it left main/master, or base; all = whole system) and plan list work items: guide:<lang>:<page>, translation:<lang>:<page>, process:<lang>:<slug>, shot:<id>, doc:<section>, tests:<target>.\n2. For each item call facts first and write only from them: the page's real labels, literal texts, endpoints, rules, messages, menu path. Never invent a field, button or message: write_doc refuses **bold** UI names the page does not have.\n3. Guides: write the primary language first (page template: What this page is for, Before you start, one section per action from facts.actions (Add, Edit, Delete, Approve…) with numbered steps and what the user should see afterwards — the success message, the changed list, the validation errors — Fields table, Messages you may see, Related guides; take every screenshot in facts.screenshot_plan), then translations (translation items; same structure, labels in that language). Screenshots: shoot a spec (path, params for :id, steps, crop, user, lang), then reference the returned image path in the guide with ![alt](guide/img/<lang>/<id>.png). Mask real data with [screenshots] mask.\n4. Portals: the app's sign-in areas (admin, selfcare, a Filament panel, a member portal…), found from its login pages; each has its own guide and its own sign-in. facts gives the page's source_path inside its portal; screenshots use the portal's session (public pages none); when shoot says a portal is not signed in, call login with that portal. The guide folder is published alone at the publish path, so guide pages never link to the start page or system document, and images live in guide/img/<lang>/.\n5. Server setup (doc:index): from facts only (versions, extensions, env keys, services, deploy files) for a real server, not localhost; mark any step the repo does not back with a line <!-- unverified --> before it.\n6. Tests: cases lists the expected cases of a target and which test covers each. write_test adds ONE parameterised test method per target with a data provider: rows keyed by case (ok, guest, forbidden, not_found, invalid:<field>:<rule>, boundary:<field>:min|max, filter:<param>, state:<name>, throws:<Exception>, returns:<name>), covering positive and negative cases; later calls add rows (leave method_php out). A case already covered is refused. Failing rows are removed and reported; after reading the code, if the code is wrong, send the row again with bug set: it is kept in group doctest-bug for the developer.\nIf doctest itself gives a wrong or confusing answer, call report_problem; failures inside doctest are recorded automatically. Either way tell the person the report id and that `doctest reports --send <id>` files it as a GitHub issue. Run doctor when something about the setup fails.\n7. issues lists code to fix as points (file:line, reason, fix): N+1, missing indexes, unoptimised queries and Vue code, failing tests. verify_changes validates the current change (affected tests, doctest tests, problems on changed lines, what is left). Tell the user about the points.\n8. Parallel work: to go faster, start several workers (e.g. 4 subagents); each calls claim with its own worker name to get its own items, works them, and claims again until none are left. Test runs are serialized by doctest. Long calls report progress (percent).\n9. check, then finish (records the commits so the next changes scope starts here).";

/// Makes a codemap client for a project; the number is the parallel worker (0 = the main client).
pub type Factory = std::sync::Arc<dyn Fn(&Config, usize) -> Box<dyn CodemapApi + Send> + Send + Sync>;

struct Project {
    cfg: Config,
    api: Box<dyn CodemapApi + Send>,
    shooter: Option<Shooter>,
    /// Portals detected from the login pages (when .doctest.toml names none), once per session.
    portals: Option<Vec<crate::config::Portal>>,
}

/// The project's config with its portals: from .doctest.toml, else detected (and cached).
fn effective_cfg(p: &mut Project) -> Result<Config, String> {
    if !p.cfg.guide.portals.is_empty() {
        return Ok(p.cfg.clone());
    }
    if p.portals.is_none() {
        let cfg = p.cfg.clone();
        let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
        let mut fb = FactBuilder::new(&mut *p.api, &cfg, &labels);
        fb.pages()?;
        p.portals = Some(fb.portals.clone());
    }
    Ok(p.cfg.with_portals(p.portals.clone().unwrap_or_default()))
}

pub struct Server {
    factory: Factory,
    projects: HashMap<PathBuf, Project>,
    /// Where replies and notifications go.
    out: crate::progress::Shared,
    /// Where problem reports are kept (~/.doctest).
    pub reports_root: PathBuf,
    told_update: bool,
}

fn s<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn pretty(v: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    let mut p = props.as_object().cloned().unwrap_or_default();
    p.insert("project".into(), json!({"type": "string", "description": "Backend repo path (holds .doctest.toml); default: the current folder"}));
    json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": p, "required": required}})
}

pub fn tools() -> Value {
    let scope = json!({"type": "string", "enum": ["changes", "branch", "all"], "description": "changes (default): since the last finish; branch: what this branch changed since it left base; all: whole system"});
    let base = json!({"type": "string", "description": "with scope branch: the base branch (default origin/HEAD, main, master, develop)"});
    json!([
        tool(
            "status",
            "Counts and the first items of what is missing or stale: guides, translations, process guides, screenshots, system document sections, test cases.",
            json!({"scope": scope, "base": base, "limit": {"type": "integer"}, "redo": {"type": "boolean", "description": "list every page guide again (rewrite all), not only missing or stale ones"}}),
            &[]
        ),
        tool(
            "plan",
            "The ordered work items (id, kind, reason). Work through them with facts → write_doc / shoot / write_test.",
            json!({"scope": scope, "base": base, "limit": {"type": "integer"}, "redo": {"type": "boolean", "description": "list every page guide again (rewrite all), not only missing or stale ones"}}),
            &[]
        ),
        tool(
            "claim",
            "For parallel AI workers: hands this worker its own next work items (not claimed by others; claims expire after 30 minutes). Refreshes the queue from a plan when empty or refresh is true.",
            json!({"worker": {"type": "string"}, "count": {"type": "integer"}, "kinds": {"type": "array", "items": {"type": "string"}}, "refresh": {"type": "boolean"}, "redo": {"type": "boolean"}, "scope": scope, "base": base}),
            &["worker"]
        ),
        tool(
            "facts",
            "Everything needed for one work item: page labels/literals/endpoints/rules/messages/menu path, current and primary text, a skeleton, the source path; server facts for doc:index; cases for tests:<target>.",
            json!({"item": {"type": "string"}}),
            &["item"]
        ),
        tool(
            "write_doc",
            "Store a guide, translation, process guide or system section (Markdown with a --- front matter header). Refused with the problems when it names UI the page lacks or links break; else saved and the HTML rebuilt.",
            json!({"item": {"type": "string"}, "markdown": {"type": "string"}}),
            &["item", "markdown"]
        ),
        tool(
            "shoot",
            "Take (or retake) a screenshot from a spec {id, page, path, params, user, steps, crop, mask, lang} or a stored spec id. Returns the image and the path to reference.",
            json!({"spec": {"type": "object"}, "id": {"type": "string"}}),
            &[]
        ),
        tool(
            "cases",
            "Expected test cases of a target (`METHOD uri` or `App\\Class::method`), each with the test that covers it or missing.",
            json!({"target": {"type": "string"}}),
            &["target"]
        ),
        tool(
            "write_test",
            "Add data-provider rows (and the parameterised method the first time) for a target, run them, keep passing rows, remove failing ones (unless bug is set).",
            json!({"target": {"type": "string"}, "method": {"type": "string"}, "method_php": {"type": "string"}, "rows": {"type": "array", "items": {"type": "object", "properties": {"case": {"type": "string"}, "args_php": {"type": "string"}, "bug": {"type": "string"}}, "required": ["case", "args_php"]}}, "remove": {"type": "array", "items": {"type": "string"}, "description": "case keys of obsolete rows to delete"}, "replace": {"type": "boolean", "description": "replace the existing doctest method with method_php (fix a broken body)"}}),
            &["target", "method", "rows"]
        ),
        tool(
            "run_tests",
            "Run doctest's tests (or a PHPUnit --filter) and record failures.",
            json!({"filter": {"type": "string"}}),
            &[]
        ),
        tool("build", "Rebuild all HTML under the document folder.", json!({}), &[]),
        tool(
            "check",
            "Check guides against pages, links, images, env keys, failing doctest tests; lists known-bug tests and code points.",
            json!({}),
            &[]
        ),
        tool(
            "issues",
            "Code to fix as points (file:line — problem: reason. Fix: …): N+1, missing indexes, unoptimised queries and Vue code, failing tests.",
            json!({"scope": scope, "base": base}),
            &[]
        ),
        tool(
            "verify_changes",
            "Validate the current changes (since the last finish, or the whole branch with scope branch): run affected tests and doctest tests, list problems on changed lines and what is left to document or test; pass or fail.",
            json!({"scope": scope, "base": base}),
            &[]
        ),
        tool(
            "finish",
            "Record this run: the commits of every repo (next changes scope starts here) and a changelog entry.",
            json!({"note": {"type": "string"}}),
            &[]
        ),
        tool(
            "report_problem",
            "Report a problem with doctest itself (a wrong or confusing answer, a missing feature, a fix that did not work). It is kept on this machine, scrubbed of paths and names; the person decides whether to file it as a GitHub issue with `doctest reports --send <id>`.",
            json!({"kind": {"type": "string", "description": "wrong-answer, confusing-answer, missing-feature, fix-did-not-work, other"}, "summary": {"type": "string"}, "step": {"type": "string", "description": "the doctest tool it happened in"}, "target": {"type": "string"}, "expected": {"type": "string"}, "actual": {"type": "string"}}),
            &["kind", "summary"]
        ),
        tool(
            "doctor",
            "Check what doctest needs (codemap, git, Chrome, gh, settings, apps, languages, test login, test command, state) and what to fix.",
            json!({}),
            &[]
        ),
        tool(
            "login",
            "Open a Chrome window on the app's sign-in page for the person to sign in by hand (OTP is fine); screenshots then reuse that session. Call it when shoot says it is not signed in or the session expired, and tell the person a window is opening.",
            json!({"portal": {"type": "string", "description": "the portal to sign in to (admin, selfcare …); default: the next one not signed in"}}),
            &[]
        ),
    ])
}

impl Server {
    #[cfg(test)]
    pub fn new(factory: Factory) -> Server {
        Server::with_output(
            factory,
            std::sync::Arc::new(std::sync::Mutex::new(Box::new(std::io::sink()))),
        )
    }

    pub fn with_output(factory: Factory, out: crate::progress::Shared) -> Server {
        Server {
            factory,
            projects: HashMap::new(),
            out,
            reports_root: crate::reports::tool_root(),
            told_update: false,
        }
    }

    pub fn live_with(out: crate::progress::Shared) -> Server {
        if let Ok(dir) = std::env::var("DOCTEST_CODEMAP_FIXTURES") {
            let dir = PathBuf::from(dir);
            return Server::with_output(
                std::sync::Arc::new(move |_c: &Config, _n: usize| -> Box<dyn CodemapApi + Send> {
                    Box::new(crate::codemap::FixtureCodemap::new(dir.clone()))
                }),
                out,
            );
        }
        Server::with_output(
            std::sync::Arc::new(|c: &Config, n: usize| -> Box<dyn CodemapApi + Send> {
                Box::new(if n == 0 {
                    LiveCodemap::new(c)
                } else {
                    LiveCodemap::worker(c, n)
                })
            }),
            out,
        )
    }

    fn project(&mut self, args: &Value) -> Result<&mut Project, String> {
        let start = s(args, "project")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let start = std::fs::canonicalize(&start).map_err(|e| format!("{}: {e}", start.display()))?;
        let cfg = Config::find(&start)?;
        let key = cfg.root.clone();
        if !self.projects.contains_key(&key) {
            let api = (self.factory)(&cfg, 0);
            self.projects.insert(
                key.clone(),
                Project {
                    cfg,
                    api,
                    shooter: None,
                    portals: None,
                },
            );
        } else {
            // pick up edits to .doctest.toml
            let p = self.projects.get_mut(&key).expect("present");
            p.cfg = cfg;
        }
        Ok(self.projects.get_mut(&key).expect("inserted"))
    }

    pub fn handle(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let params = message.get("params").cloned().unwrap_or(json!({}));
        let answer = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        Some(match message["method"].as_str().unwrap_or_default() {
            "initialize" => answer(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}, "prompts": {}},
                "serverInfo": {"name": "doctest", "version": env!("CARGO_PKG_VERSION")},
                "instructions": MCP_INSTRUCTIONS,
            })),
            "ping" => answer(json!({})),
            "tools/list" => answer(json!({"tools": tools()})),
            "prompts/list" => answer(json!({"prompts": prompts()})),
            "prompts/get" => {
                let name = params["name"].as_str().unwrap_or_default();
                let words: Vec<String> = params["arguments"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(_, v)| v.as_str())
                    .flat_map(|v| v.split_whitespace().map(String::from).collect::<Vec<_>>())
                    .collect();
                match prompt_text(name, &words) {
                    Some(text) => answer(
                        json!({"description": format!("doctest {name}"), "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}),
                    ),
                    None => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("unknown prompt {name}; prompts: all, branch, changes")}})
                    }
                }
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Object(Map::new()));
                let token = params["_meta"]["progressToken"].clone();
                let progress: Box<dyn crate::progress::Progress> = if token.is_null() {
                    Box::new(crate::progress::Silent)
                } else {
                    Box::new(crate::progress::Mcp::new(token, self.out.clone()))
                };
                let started = std::time::Instant::now();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.call(&name, &args, &*progress)))
                        .unwrap_or_else(|_| Err(format!("the tool {name} panicked")));
                match result {
                    Ok(mut content) => {
                        if !self.told_update
                            && let Some(newer) = crate::update::known_newer()
                        {
                            self.told_update = true;
                            content.push(json!({"type": "text", "text": format!("(doctest {newer} is available; this server is {}. Tell the person: run `doctest update`.)", crate::update::VERSION)}));
                        }
                        let all: String = content
                            .iter()
                            .filter_map(|c| c["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        content.push(json!({"type": "text", "text": summary(&name, Ok(&all), started.elapsed())}));
                        answer(json!({"content": content, "isError": false}))
                    }
                    Err(mut e) => {
                        if crate::reports::is_internal(&e) {
                            let target = s(&args, "target").or_else(|| s(&args, "item"));
                            let id = crate::reports::add_at(
                                &self.reports_root,
                                crate::reports::New {
                                    kind: report_kind(&e),
                                    source: "auto",
                                    step: &name,
                                    target,
                                    summary: &e,
                                    expected: None,
                                    actual: None,
                                },
                            );
                            e += &format!(
                                "\n(doctest recorded this as problem report {id}; tell the person they can file it with `doctest reports --send {id}`.)"
                            );
                        }
                        let footer = summary(&name, Err(&e), started.elapsed());
                        answer(
                            json!({"content": [{"type": "text", "text": format!("{e}\n\n{footer}")}], "isError": true}),
                        )
                    }
                }
            }
            other => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}})
            }
        })
    }

    fn call(
        &mut self,
        name: &str,
        args: &Value,
        progress: &dyn crate::progress::Progress,
    ) -> Result<Vec<Value>, String> {
        let text = |t: String| Ok(vec![json!({"type": "text", "text": t})]);
        let factory = self.factory.clone();
        if name == "report_problem" {
            let id = crate::reports::add_at(
                &self.reports_root,
                crate::reports::New {
                    kind: s(args, "kind").unwrap_or("other"),
                    source: "ai",
                    step: s(args, "step").unwrap_or("unknown"),
                    target: s(args, "target"),
                    summary: s(args, "summary").ok_or("summary is required")?,
                    expected: s(args, "expected"),
                    actual: s(args, "actual"),
                },
            );
            return text(format!(
                "recorded problem report {id} on this machine (scrubbed). Tell the person: they can file it as a GitHub issue with `doctest reports --send {id}` (they see it first)."
            ));
        }
        if name == "doctor" {
            let start = s(args, "project")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let start = std::fs::canonicalize(&start).unwrap_or(start);
            let cfg = Config::find(&start).ok();
            return text(crate::doctor::render(&crate::doctor::run(cfg.as_ref(), &start)));
        }
        let p = self.project(args)?;
        let scope = Scope::parse(s(args, "scope"), s(args, "base"))?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(40) as usize;
        match name {
            "status" | "plan" => {
                let mut st = State::load(&p.cfg);
                let mut plan = run_plan(&factory, p, &mut st, &scope, progress)?;
                if args.get("redo").and_then(Value::as_bool).unwrap_or(false) {
                    work::add_redo(&mut plan, &p.cfg, &st);
                }
                if name == "status" {
                    text(pretty(&work::status(&plan, &scope, limit)))
                } else {
                    text(pretty(&plan.items.iter().take(limit).collect::<Vec<_>>()))
                }
            }
            "claim" => {
                let worker = s(args, "worker")
                    .ok_or("worker is required (any name, e.g. w1)")?
                    .to_string();
                let count = args.get("count").and_then(Value::as_u64).unwrap_or(5) as usize;
                let kinds: Vec<String> = args
                    .get("kinds")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false)
                    || State::load(&p.cfg).queue.is_empty();
                if refresh {
                    let mut st = State::load(&p.cfg);
                    let mut plan = run_plan(&factory, p, &mut st, &scope, progress)?;
                    if args.get("redo").and_then(Value::as_bool).unwrap_or(false) {
                        work::add_redo(&mut plan, &p.cfg, &st);
                    }
                    let items: Vec<&work::WorkItem> = plan
                        .items
                        .iter()
                        .filter(|i| kinds.is_empty() || kinds.iter().any(|k| i.id.starts_with(&format!("{k}:"))))
                        .collect();
                    State::update(&p.cfg, |s| {
                        s.queue = items.iter().map(|i| i.id.clone()).collect();
                        s.reasons = items.iter().map(|i| (i.id.clone(), i.reason.clone())).collect();
                        let q = s.queue.clone();
                        s.claims.retain(|k, _| q.contains(k));
                    })?;
                }
                let (got, left) = State::update(&p.cfg, |s| {
                    let got: Vec<(String, String)> = s
                        .claim(&worker, count, crate::state::now())
                        .into_iter()
                        .map(|id| {
                            let r = s.reasons.get(&id).cloned().unwrap_or_default();
                            (id, r)
                        })
                        .collect();
                    (got, s.queue.len())
                })?;
                text(pretty(
                    &json!({"worker": worker, "items": got.iter().map(|(id, r)| json!({"id": id, "reason": r})).collect::<Vec<_>>(), "queued": left, "note": "work these items (facts → write_doc / shoot / write_test); a written item leaves the queue; claims expire after 30 minutes"}),
                ))
            }
            "facts" => text(facts(p, s(args, "item").ok_or("item is required")?)?),
            "write_doc" => text(write_doc(
                p,
                s(args, "item").ok_or("item is required")?,
                s(args, "markdown").ok_or("markdown is required")?,
            )?),
            "shoot" => shoot(p, args),
            "cases" => text(cases_text(p, s(args, "target").ok_or("target is required")?)?),
            "write_test" => text(write_test(p, args)?),
            "run_tests" => text(run_tests(p, s(args, "filter"))?),
            "build" => {
                let sources = source::sources(&p.cfg)?;
                let r = crate::html::build(&p.cfg, &sources, &State::load(&p.cfg))?;
                text(format!("built {} files in {}", r.written.len(), p.cfg.out.display()))
            }
            "check" => {
                let mut st = State::load(&p.cfg);
                let r = crate::check::run(&p.cfg, &mut *p.api, &mut st, progress)?;
                let points: Vec<String> = r.points.iter().map(crate::issues::Issue::point).collect();
                text(pretty(
                    &json!({"passed": r.passed(), "errors": r.errors, "warnings": r.warnings, "known_bugs": r.bugs, "code_to_fix": points}),
                ))
            }
            "login" => {
                let eff = effective_cfg(p)?;
                let targets = eff.login_targets();
                if targets.is_empty() {
                    return text("this app has no sign-in page: screenshots open its pages directly".into());
                }
                let wanted = s(args, "portal").or_else(|| s(args, "user"));
                let name = match wanted {
                    Some(w) => targets
                        .iter()
                        .find(|(n, _)| n == w)
                        .map(|(n, _)| n.clone())
                        .ok_or_else(|| {
                            format!(
                                "unknown portal {w}; portals with a sign-in page: {}",
                                targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                            )
                        })?,
                    None => targets
                        .iter()
                        .find(|(n, _)| !crate::shots::signed_in(&eff, n))
                        .map(|(n, _)| n.clone())
                        .ok_or("already signed in to every portal")?,
                };
                progress.step(
                    0,
                    1,
                    &format!("waiting for the person to sign in to the {name} portal in the Chrome window"),
                );
                let u = crate::shots::login(&eff, &name, false, &|_| {})?;
                p.shooter = None; // the next shoot starts from the new session
                progress.step(1, 1, "signed in");
                let left: Vec<String> = targets
                    .iter()
                    .filter(|(n, _)| !crate::shots::signed_in(&eff, n))
                    .map(|(n, _)| n.clone())
                    .collect();
                text(format!(
                    "signed in to the {u} portal; screenshots of its pages reuse this session{}",
                    if left.is_empty() {
                        String::new()
                    } else {
                        format!(". Still to sign in: {}", left.join(", "))
                    }
                ))
            }
            "issues" => {
                let mut points = Vec::new();
                match scope {
                    Scope::All => {
                        let mut sc = IssueScanner::new(&mut *p.api, &p.cfg);
                        points.extend(sc.scan_files_with(&crate::issues::app_files(&p.cfg), progress)?);
                        for fe in &p.cfg.frontends {
                            points.extend(frontend_issues(fe, &frontend_files(fe)));
                        }
                    }
                    Scope::Changes { .. } | Scope::Branch { .. } => {
                        let st = State::load(&p.cfg);
                        let since = match work::since_of(&p.cfg, &st, &scope)? {
                            Some(s) => s,
                            None => work::commits(&p.cfg)?,
                        };
                        let changed = work::changed_files(&p.cfg, &since)?;
                        let be = changed
                            .get(&Config::repo_name(&p.cfg.backend))
                            .cloned()
                            .unwrap_or_default();
                        let php: Vec<PathBuf> = be
                            .into_iter()
                            .filter(|f| f.starts_with("app") && f.extension().is_some_and(|x| x == "php"))
                            .collect();
                        let mut sc = IssueScanner::new(&mut *p.api, &p.cfg);
                        points.extend(sc.scan_files(&php)?);
                        for fe in &p.cfg.frontends {
                            let files: Vec<PathBuf> = changed
                                .get(&Config::repo_name(fe))
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|f| f.starts_with(Config::src_rel(fe)) && !f.starts_with("node_modules"))
                                .collect();
                            points.extend(frontend_issues(fe, &files));
                        }
                    }
                }
                let lines: Vec<String> = points.iter().map(crate::issues::Issue::point).collect();
                text(if lines.is_empty() {
                    "no code problems found".into()
                } else {
                    format!("{} points to fix:\n- {}", lines.len(), lines.join("\n- "))
                })
            }
            "verify_changes" => {
                let mut st = State::load(&p.cfg);
                let since = match scope {
                    Scope::Branch { .. } => work::since_of(&p.cfg, &st, &scope)?,
                    _ => None,
                };
                let r = crate::verify::run(&p.cfg, &mut *p.api, &mut st, since, progress)?;
                State::update(&p.cfg, |s| s.merge_plan(&st))?;
                let points: Vec<String> = r.errors.iter().map(crate::issues::Issue::point).collect();
                text(pretty(
                    &json!({"pass": r.pass, "to_fix": points, "still_to_do": r.todo, "tests_run": r.tests_run, "test_output_tail": r.test_output}),
                ))
            }
            "finish" => {
                let commits = work::commits(&p.cfg)?;
                let note = s(args, "note").unwrap_or_default().to_string();
                let st = State::update(&p.cfg, |st| {
                    st.last_commit = commits.clone();
                    let updated = std::mem::take(&mut st.written);
                    let seconds = st.run_started.take().map(|t| crate::state::now().saturating_sub(t));
                    st.runs.push(RunRecord {
                        at: chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
                        commits,
                        updated,
                        note,
                        seconds,
                    });
                    st.queue.clear();
                    st.claims.clear();
                    st.reasons.clear();
                    st.clone()
                })?;
                let sources = source::sources(&p.cfg)?;
                crate::html::build(&p.cfg, &sources, &st)?;
                let last = st.runs.last();
                let took = last
                    .and_then(|r| r.seconds)
                    .map(|s| format!("took {}; ", work::duration(s)))
                    .unwrap_or_default();
                let written = last.map(|r| r.updated.len()).unwrap_or(0);
                text(format!(
                    "finished: {took}{written} item(s) written; commits recorded for {} repo(s); the next changes scope starts here",
                    st.last_commit.len()
                ))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

/// The user a screenshot signs in as: its own, else its portal's, else the first configured user.
/// The session a screenshot uses: its own, else its page's portal (public pages sign in nowhere), else the default.
fn shot_user(cfg: &Config, spec: &ShotSpec) -> Option<String> {
    spec.user.clone().or_else(|| {
        let portal = cfg.portal_of(&spec.page, &spec.path);
        (!portal.is_empty()).then_some(portal)
    })
}

fn report_kind(error: &str) -> &'static str {
    let e = error.to_lowercase();
    if e.contains("panicked") {
        "panic"
    } else if e.contains("codemap") {
        "codemap-failure"
    } else if e.contains("ran no tests") {
        "unreadable-test-output"
    } else if e.contains("chrome") || e.contains("screenshot") {
        "screenshot-failure"
    } else {
        "internal-error"
    }
}

fn took(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        work::duration(d.as_secs())
    }
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// Lines of `text` that start with "- " after `after` (or anywhere when empty).
fn bullets(text: &str, after: &str) -> Vec<String> {
    let start = if after.is_empty() {
        0
    } else {
        text.find(after).map(|i| i + after.len()).unwrap_or(text.len())
    };
    text[start..]
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .map(|l| l.chars().take(160).collect())
        .collect()
}

fn first_json(text: &str) -> Value {
    let start = text.find(['{', '[']).unwrap_or(0);
    let mut de = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
    de.next().and_then(Result::ok).unwrap_or(Value::Null)
}

fn listed(items: &[String], max: usize) -> String {
    let mut s = items.iter().take(max).cloned().collect::<Vec<_>>().join("; ");
    if items.len() > max {
        s += &format!("; … {} more", items.len() - max);
    }
    s
}

/// The summary every tool result ends with: how long it took, what it found, what to do next.
pub fn summary(tool: &str, outcome: Result<&str, &str>, elapsed: std::time::Duration) -> String {
    let (head, found, next) = match outcome {
        Err(e) => {
            let first = e.lines().next().unwrap_or(e).chars().take(200).collect::<String>();
            let next = if e.contains("problem report") {
                "tell the person the problem report id; then try again or move on to another item"
            } else if e.contains("already covered") {
                "drop those rows (existing tests cover them) and send the rest"
            } else if e.contains("rolled back") {
                "fix method_php (or the rows) as the message says and send write_test again"
            } else if e.contains("doctest login") {
                "call the login tool (a Chrome window opens for the person to sign in), then shoot again"
            } else if e.contains(" is required")
                || e.contains("bad item")
                || e.contains("not in [project]")
                || e.contains("unknown")
            {
                "fix the request as the message says and call it again"
            } else {
                "read the message and fix it; if it is about setup (codemap, Chrome, login, settings), run doctor"
            };
            (
                format!("{tool} failed after {}", took(elapsed)),
                first,
                next.to_string(),
            )
        }
        Ok(text) => {
            let (found, next): (String, String) = match tool {
                "status" | "plan" => {
                    let v = first_json(text);
                    let items: Vec<String> = if tool == "status" {
                        v["counts"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(k, n)| format!("{n} {k}"))
                            .collect()
                    } else {
                        let mut m: BTreeMap<String, usize> = BTreeMap::new();
                        for i in v.as_array().into_iter().flatten() {
                            *m.entry(i["kind"].as_str().unwrap_or("item").to_string()).or_default() += 1;
                        }
                        m.into_iter().map(|(k, n)| format!("{n} {k}")).collect()
                    };
                    if items.is_empty() {
                        (
                            "nothing missing or out of date".into(),
                            "run issues and check, then finish".into(),
                        )
                    } else {
                        (
                            format!("to do: {}", items.join(", ")),
                            "work the items: claim (or facts on the first item), then write each one".into(),
                        )
                    }
                }
                "claim" => {
                    let v = first_json(text);
                    let n = v["items"].as_array().map(Vec::len).unwrap_or(0);
                    if n == 0 {
                        ("no items left".into(), "issues, check, build, then finish".into())
                    } else {
                        (
                            format!("{} claimed, {} still queued", plural(n, "item"), v["queued"]),
                            "facts for each item, then write it (write_doc / shoot / write_test)".into(),
                        )
                    }
                }
                "facts" => {
                    let v = first_json(text);
                    if let Some(cases) = v["cases"].as_array() {
                        let missing = cases.iter().filter(|c| c["covered_by"].is_null()).count();
                        (
                            format!(
                                "{} missing, {} already covered",
                                plural(missing, "case"),
                                cases.len() - missing
                            ),
                            if missing == 0 {
                                "nothing to write for this target; next item".into()
                            } else {
                                "write_test: one method with a data provider, a row per missing case".into()
                            },
                        )
                    } else if let Some(plan) = v["screenshot_plan"].as_array() {
                        let actions = v["actions"].as_array().map(Vec::len).unwrap_or(0);
                        (
                            format!(
                                "{}, {} planned",
                                plural(actions, "action"),
                                plural(plan.len(), "screenshot")
                            ),
                            "shoot each screenshot in screenshot_plan, then write_doc at source_path".into(),
                        )
                    } else {
                        ("facts ready".into(), "write_doc from these facts".into())
                    }
                }
                "cases" => {
                    let v = first_json(text);
                    let cases = v["cases"].as_array().cloned().unwrap_or_default();
                    let missing = cases.iter().filter(|c| c["covered_by"].is_null()).count();
                    (
                        format!("{} missing, {} covered", plural(missing, "case"), cases.len() - missing),
                        if missing == 0 {
                            "next item".into()
                        } else {
                            "write_test with a row per missing case".into()
                        },
                    )
                }
                "write_doc" => {
                    if text.starts_with("nothing was saved") {
                        let errs = bullets(text, "");
                        (
                            format!("{}: {}", plural(errs.len(), "error"), listed(&errs, 3)),
                            "fix those and send write_doc again".into(),
                        )
                    } else {
                        let warns = bullets(text, "warnings:");
                        (
                            if warns.is_empty() {
                                "nothing wrong".into()
                            } else {
                                format!("{}: {}", plural(warns.len(), "warning"), listed(&warns, 3))
                            },
                            "next item: claim (or facts on the next item); finish when none are left".into(),
                        )
                    }
                }
                "shoot" => (
                    "nothing wrong".into(),
                    "put the image in the guide with the path above, then continue".into(),
                ),
                "write_test" => {
                    let failed: Vec<String> = text
                        .lines()
                        .filter_map(|l| l.strip_prefix("- "))
                        .filter(|l| l.contains("failed and was removed"))
                        .map(|l| l.split(" failed").next().unwrap_or(l).to_string())
                        .collect();
                    let not_bugs = text.contains("marked as bugs but passing");
                    if failed.is_empty() && !not_bugs {
                        (
                            "all rows pass".into(),
                            "next item: claim; finish when none are left".into(),
                        )
                    } else {
                        let mut f = Vec::new();
                        if !failed.is_empty() {
                            f.push(format!(
                                "{} failed and removed: {}",
                                plural(failed.len(), "row"),
                                listed(&failed, 5)
                            ));
                        }
                        if not_bugs {
                            f.push("rows marked as bugs pass (not bugs)".into());
                        }
                        (
                            f.join("; "),
                            "read the code for each failed case: fix the row, or send it again with bug set if the code is wrong".into(),
                        )
                    }
                }
                "run_tests" => {
                    let line = text.lines().next().unwrap_or_default().to_string();
                    let failing = !line.contains(" 0 failed") && line.contains("failed");
                    (
                        line,
                        if failing {
                            "fix the failing tests listed (file:line)".into()
                        } else {
                            "continue".into()
                        },
                    )
                }
                "build" => ("nothing wrong".into(), "check".into()),
                "check" => {
                    let v = first_json(text);
                    let n = |k: &str| v[k].as_array().map(Vec::len).unwrap_or(0);
                    let first = v["errors"][0]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(140)
                        .collect::<String>();
                    (
                        format!(
                            "{}, {}, {}, {}",
                            plural(n("errors"), "error"),
                            plural(n("warnings"), "warning"),
                            plural(n("known_bugs"), "known bug"),
                            plural(n("code_to_fix"), "code point")
                        ),
                        if n("errors") > 0 {
                            format!("fix the errors (first: {first}), then check again")
                        } else {
                            "finish; tell the person about the code points".into()
                        },
                    )
                }
                "issues" => {
                    let n = text
                        .split_whitespace()
                        .next()
                        .and_then(|w| w.parse::<usize>().ok())
                        .unwrap_or(0);
                    (
                        if n == 0 {
                            "no code problems".into()
                        } else {
                            format!("{} to fix", plural(n, "code point"))
                        },
                        if n == 0 {
                            "continue".into()
                        } else {
                            "tell the person the points (file:line, reason, fix); fix them if they ask".into()
                        },
                    )
                }
                "verify_changes" => {
                    let v = first_json(text);
                    let fix = v["to_fix"].as_array().map(Vec::len).unwrap_or(0);
                    let todo = v["still_to_do"].as_array().map(Vec::len).unwrap_or(0);
                    (
                        format!(
                            "{}: {}, {} still to document or test",
                            if v["pass"] == true { "pass" } else { "fail" },
                            plural(fix, "point to fix"),
                            plural(todo, "item")
                        ),
                        if fix > 0 {
                            "fix the points (file:line), then verify_changes again".into()
                        } else if todo > 0 {
                            "work the remaining items (claim)".into()
                        } else {
                            "finish".into()
                        },
                    )
                }
                "finish" => (
                    "run recorded".into(),
                    "tell the person: how long it took, what was written, the code points and any problem reports"
                        .into(),
                ),
                "doctor" => {
                    let fails = text.lines().filter(|l| l.starts_with("FAIL")).count();
                    let warns = text.lines().filter(|l| l.starts_with("warn")).count();
                    (
                        format!("{}, {}", plural(fails, "problem"), plural(warns, "warning")),
                        if fails > 0 {
                            "fix the FAIL lines first".into()
                        } else {
                            "continue".into()
                        },
                    )
                }
                "login" => ("signed in".into(), "shoot the screenshots again".into()),
                "report_problem" => (
                    "problem recorded".into(),
                    "tell the person the id and `doctest reports --send <id>`".into(),
                ),
                _ => ("done".into(), "continue".into()),
            };
            (format!("{tool} took {}", took(elapsed)), found, next)
        }
    };
    format!("— doctest: {head}\nfound: {found}\nnext: {next}")
}

pub fn prompts() -> Value {
    json!([
        {"name": "all", "description": "Document and test the whole application: every guide, screenshot, translation, system section and test case that is missing or out of date. Options: a number of workers (default 1), `redo` to rewrite every guide.",
         "arguments": [{"name": "options", "description": "optional: workers (e.g. 4) and/or redo", "required": false}]},
        {"name": "branch", "description": "Document and test only what this git branch changed (compared with main/master). Options: a base branch, a number of workers (default 1).",
         "arguments": [{"name": "options", "description": "optional: base branch and/or workers", "required": false}]},
        {"name": "changes", "description": "Document and test what changed since the last finished doctest run. Option: a number of workers (default 1).",
         "arguments": [{"name": "options", "description": "optional: workers", "required": false}]},
    ])
}

/// The instructions behind /mcp__doctest__<name>: the whole workflow, so a person only types the command.
pub fn prompt_text(name: &str, words: &[String]) -> Option<String> {
    let workers: usize = words.iter().find_map(|w| w.parse().ok()).unwrap_or(1).clamp(1, 16);
    let redo = name == "all" && words.iter().any(|w| w.eq_ignore_ascii_case("redo"));
    let base = if name == "branch" {
        words
            .iter()
            .find(|w| w.parse::<usize>().is_err() && !w.eq_ignore_ascii_case("redo"))
            .cloned()
    } else {
        None
    };
    let (what, args) = match name {
        "all" => (
            "the whole application",
            format!("\"scope\": \"all\"{}", if redo { ", \"redo\": true" } else { "" }),
        ),
        "branch" => (
            "what this git branch changed",
            format!(
                "\"scope\": \"branch\"{}",
                base.as_ref()
                    .map(|b| format!(", \"base\": \"{b}\""))
                    .unwrap_or_default()
            ),
        ),
        "changes" => (
            "what changed since the last finished doctest run",
            "\"scope\": \"changes\"".to_string(),
        ),
        _ => return None,
    };
    let scope_word = match name {
        "all" => "all",
        "branch" => "branch",
        _ => "changes",
    };
    let work = if workers == 1 {
        format!(
            "Use 1 worker (you): call claim with {{\"worker\": \"w1\", \"count\": 5, {args}}}, work every item it gives you, and claim again until no items are left."
        )
    } else {
        format!(
            "Use {workers} workers: start {workers} subagents at the same time, named w1 … w{workers}, each with the item rules below. Each calls claim with {{\"worker\": \"<its name>\", \"count\": 5, {args}}}, works its items, and claims again until none are left. Wait for all of them before step 4."
        )
    };
    Some(format!(
        "Document and test {what} with doctest (scope \"{scope_word}\"{redo_note}). Keep going without asking me unless you are blocked. Every doctest reply ends with a summary (took / found / next): follow its next line, and keep the times and findings for your report.

1. Call the doctor tool. If there is no .doctest.toml, run `doctest init` in the Laravel repo, then fill its TODO lines from the project itself (codemap: the login page and its fields, locale files, menu file, portals from route-name prefixes, the test command and container, the app's local URL). If shoot says it is not signed in (or the session expired), call the login tool: a Chrome window opens for me to sign in by hand; tell me it is opening and wait.
2. Call status with {{{args}}} and tell me the counts and the progress percentages.
3. {work}
   Item rules — for every item call facts first, then:
   - guide:… / translation:… — take every screenshot in facts.screenshot_plan with shoot (masks hide real data), then write_doc. The primary language first, then translations.
   - process:… — write_doc for the process across the pages it covers.
   - doc:… — write_doc from the facts (the start page is for a real server, not localhost).
   - shot:… — shoot it again with its stored spec (fix selectors if the page changed).
   - tests:… — cases, then write_test: one method with a data provider per target, a row per missing case; remove the obsolete rows it lists.
   When write_doc or write_test refuses, fix what it says and send it again; never invent a field, button, endpoint or message.
4. Call issues (same scope), then check, then build.
5. Call finish and report: the time it took (finish says), guides, screenshots and translations written, test rows written, removed or kept as bugs, the code points from issues, any problem reports (ids), and the path of document/guide/index.html.",
        redo_note = if redo { ", rewriting every guide" } else { "" }
    ))
}

/// A plan with the project's workers (each with its own codemap client), merged into the saved state.
fn run_plan(
    factory: &Factory,
    p: &mut Project,
    st: &mut State,
    scope: &Scope,
    progress: &dyn crate::progress::Progress,
) -> Result<work::Plan, String> {
    let cfg = p.cfg.clone();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let make = || -> Box<dyn CodemapApi + Send> {
        factory(
            &cfg,
            next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % cfg.workers.max(1) + 1,
        )
    };
    let plan = work::plan_parallel(&p.cfg, &mut *p.api, st, scope, progress, Some(&make))?;
    State::update(&p.cfg, |s| {
        s.merge_plan(st);
        if s.run_started.is_none() {
            s.run_started = Some(crate::state::now());
        }
    })?;
    Ok(plan)
}

/// `kind:lang:rest` → (kind, lang, rest); `doc:x` → ("doc", "", x); `shot:id`, `tests:target`.
fn split_item(item: &str) -> Result<(&str, &str, &str), String> {
    let (kind, rest) = item.split_once(':').ok_or_else(|| format!("bad item {item}"))?;
    match kind {
        "guide" | "translation" | "process" => {
            let (lang, key) = rest
                .split_once(':')
                .ok_or_else(|| format!("bad item {item}: expected {kind}:<lang>:<name>"))?;
            if key.contains("..") || key.contains('\\') || key.starts_with('/') && key.contains("/../") {
                return Err(format!("bad item {item}"));
            }
            Ok((kind, lang, key))
        }
        "doc" => {
            let ok = rest == "index"
                || (rest.starts_with("system/")
                    && rest.split('/').all(|p| {
                        !p.is_empty()
                            && p != ".."
                            && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    }));
            if !ok {
                return Err(format!("bad item {item}: doc items are doc:index or doc:system/<name>"));
            }
            Ok((kind, "", rest))
        }
        "shot" | "tests" => Ok((kind, "", rest)),
        _ => Err(format!("unknown item kind {kind}")),
    }
}

fn check_lang(cfg: &Config, lang: &str) -> Result<(), String> {
    if lang.is_empty() || cfg.languages.iter().any(|l| l == lang) {
        Ok(())
    } else {
        Err(format!(
            "language {lang} is not in [project] languages ({})",
            cfg.languages.join(", ")
        ))
    }
}

fn skeleton(f: &crate::pages::PageFacts, lang: &str) -> String {
    let title = f.menu_titles.last().cloned().unwrap_or_else(|| f.page.key.clone());
    let mut md = format!(
        "---\ntitle: {title}\nkind: page\nmodule: {}\nroutes: [{}]\norder: 10\nsummary: TODO one line\n---\n# {title}\n\nTODO what this page is for.\n\n## Before you start\n\n## Step by step\n1. Open {}.\n\n## Fields\n\n| Field | What to enter | Required |\n| --- | --- | --- |\n",
        f.page.module,
        f.page.key,
        f.menu_titles
            .iter()
            .map(|t| format!("**{t}**"))
            .collect::<Vec<_>>()
            .join(" > ")
    );
    let mut fields: BTreeMap<String, bool> = BTreeMap::new();
    for e in &f.endpoints {
        for (field, rules) in &e.rules {
            let req = rules.iter().any(|r| r == "required");
            *fields.entry(field.clone()).or_default() |= req;
        }
    }
    for (field, req) in fields {
        md += &format!("| TODO label for `{field}` | | {} |\n", if req { "Yes" } else { "No" });
    }
    for a in crate::pages::actions(f) {
        md += &format!(
            "\n## {} …\n\n1. TODO steps (bold only on-screen names).\n\n**What you should see:** {}\n\n![{} result](guide/img/{lang}/{}/{}-N.png)\n",
            a.name,
            if a.success.is_empty() {
                "TODO the result on screen".to_string()
            } else {
                a.success.iter().map(|m| format!("*{m}*")).collect::<Vec<_>>().join(" ")
            },
            a.name,
            source::slug(&f.page.module),
            source::slug(&f.page.key)
        );
        if !a.required.is_empty() {
            md += &format!(
                "\nIf you leave {} empty, TODO the error you see.\n",
                a.required
                    .iter()
                    .map(|r| format!("`{r}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    md += "\n## Messages you may see\n\n";
    for e in &f.endpoints {
        for m in &e.messages {
            md += &format!("- *{m}* — TODO what it means\n");
        }
    }
    md += "\n## Related guides\n";
    md
}

fn facts(p: &mut Project, item: &str) -> Result<String, String> {
    let (kind, lang, key) = split_item(item)?;
    check_lang(&p.cfg, lang)?;
    let cfg = p.cfg.clone();
    let sources = source::sources(&cfg)?;
    let primary = cfg.primary_language().to_string();
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let mut fb = FactBuilder::new(&mut *p.api, &cfg, &labels);
    match kind {
        "guide" | "translation" => {
            let pages = fb.pages()?;
            let page = pages
                .iter()
                .find(|x| x.key == key)
                .ok_or_else(|| format!("no page {key}"))?;
            let f = fb.facts(page)?;
            let page = &f.page;
            let path = source::guide_path(lang, &page.portal, &page.module, &page.key);
            let current = work::page_guide(&sources, key, lang).map(Source::render_text);
            let primary_text = if kind == "translation" {
                work::page_guide(&sources, key, &primary).map(Source::render_text)
            } else {
                None
            };
            let st = State::load(&cfg);
            let shots: Vec<&ShotSpec> = st.shots.values().filter(|s| s.page == key).collect();
            let endpoints: Vec<Value> = f
                .endpoints
                .iter()
                .map(|e| json!({"endpoint": format!("{} {}", e.method, e.uri), "action": e.at, "permissions": e.permissions, "rules": e.rules, "messages": e.messages}))
                .collect();
            Ok(pretty(&json!({
                "item": item,
                "source_path": path,
                "page": {"key": page.key, "path": page.path, "component": page.component, "module": page.module, "menu_path": f.menu_titles},
                "labels": f.labels.get(lang),
                "literal_texts": f.literals,
                "endpoints": endpoints,
                "components": f.components,
                "current_text": current,
                "primary_text": primary_text,
                "skeleton": if kind == "guide" && current.is_none() { Some(skeleton(&f, lang)) } else { None },
                "actions": crate::pages::actions(&f),
                "screenshot_plan": crate::pages::screenshot_plan(&crate::pages::actions(&f)),
                "min_screenshots": cfg.guide.min_screenshots.unwrap_or(1 + crate::pages::actions(&f).len()),
                "screenshots": shots,
                "image_ref_format": format!("guide/img/{lang}/{}/{}-<n>.png", source::slug(&page.module), source::slug(&page.key)),
                "rules": "Bold (**...**) only on-screen names from labels, literal_texts or menu_path. Exact on-screen message texts in italics. One section per action (heading naming it) with numbered steps and 'What you should see' after Add, Edit, Delete and every other action, including the validation errors for required fields. Take the screenshots in screenshot_plan (at least min_screenshots).",
            })))
        }
        "process" => {
            let menu: Vec<Value> = fb
                .menu
                .iter()
                .map(|m| json!({"module": m.module, "group": m.group, "label": m.label, "route": m.route_name}))
                .collect();
            let guides: Vec<Value> = sources
                .iter()
                .filter(|s| s.fm.kind == Kind::Page && s.fm.lang == lang)
                .map(|s| json!({"page": s.fm.routes.first(), "title": s.fm.title, "summary": s.fm.summary, "path": s.path}))
                .collect();
            Ok(pretty(
                &json!({"item": item, "source_path_hint": format!("guide/{lang}/<module>/process-{key}.md"), "front_matter": "kind: process, routes: [the pages in order], covers: [other pages it touches]", "menu": menu, "page_guides": guides}),
            ))
        }
        "doc" => {
            let rel = if key == "index" {
                "index.md".to_string()
            } else {
                format!("{key}.md")
            };
            let current = sources
                .iter()
                .find(|s| s.path == Path::new(&rel))
                .map(Source::render_text);
            let server = crate::server::facts(&cfg);
            let extra = if key == "system/database" {
                fb.api().call(BACKEND, "get_schema", json!({}))?.data
            } else if key == "system/jobs" {
                fb.api().call(BACKEND, "get_schema", json!({"background": true}))?.data
            } else if key == "system/architecture" {
                fb.api()
                    .call(BACKEND, "project_overview", json!({"detail": "full"}))?
                    .data
            } else if key == "system/api" {
                let (routes, _) = fb
                    .api()
                    .paged(BACKEND, "list_routes", json!({"permissions": true}), "routes")?;
                json!({"routes": routes.len(), "first_300": routes.into_iter().take(300).collect::<Vec<_>>()})
            } else if let Some(m) = key.strip_prefix("system/modules/") {
                let pages = fb.pages()?;
                let list: Vec<Value> = pages.iter().filter(|x| source::slug(&x.module) == m).map(|x| {
                    let g = work::page_guide(&sources, &x.key, &primary);
                    json!({"page": x.key, "path": x.path, "guide": g.map(|g| g.path.clone()), "summary": g.map(|g| g.fm.summary.clone())})
                }).collect();
                json!({"pages": list})
            } else {
                Value::Null
            };
            let publish = if key == "index" {
                json!({
                    "path": cfg.guide.publish_path,
                    "folder": "document/guide (self-contained: its own assets, images and search)",
                    "nginx": format!("location ^~ {p}/ {{ alias <app path on the server>/document/guide/; index index.html; }}", p = cfg.guide.publish_path.trim_end_matches('/')),
                    "note": "Publish only document/guide. index.html and system/ hold server setup and env key names: keep them internal.",
                })
            } else {
                Value::Null
            };
            Ok(pretty(&json!({
                "item": item,
                "source_path": rel,
                "current_text": current,
                "publish_user_guide": publish,
                "server": server,
                "details": extra,
                "rules": "Write for a real server, not localhost. Every step must come from these facts; put <!-- unverified --> on its own line before any step the repo does not back. Mention every env key by name in index or system/configuration.",
            })))
        }
        "shot" => {
            let st = State::load(&cfg);
            Ok(pretty(
                &json!({"item": item, "spec": st.shots.get(key), "spec_format": ShotSpec::default(), "note": "shoot with the spec (fix steps or selectors first if the page changed)"}),
            ))
        }
        "tests" => {
            drop(fb);
            cases_text(p, key)
        }
        _ => Err(format!("unknown item {item}")),
    }
}

fn source_path_for(
    cfg: &Config,
    sources: &[Source],
    kind: &str,
    lang: &str,
    key: &str,
    fm_module: Option<&str>,
    pages: &[crate::pages::Page],
) -> Result<PathBuf, String> {
    let primary = cfg.primary_language();
    Ok(match kind {
        "guide" => {
            let page = pages
                .iter()
                .find(|p| p.key == key)
                .ok_or_else(|| format!("no page {key}"))?;
            let module = fm_module.unwrap_or(&page.module);
            source::guide_path(lang, &page.portal, module, key)
        }
        "translation" => {
            let p = work::page_guide(sources, key, primary)
                .map(|s| s.path.clone())
                .or_else(|| {
                    sources
                        .iter()
                        .find(|s| s.fm.lang == primary && source::slug(&s.fm.title) == key)
                        .map(|s| s.path.clone())
                });
            work::swap_lang(
                &p.ok_or_else(|| format!("write the {primary} guide for {key} first"))?,
                lang,
            )
        }
        "process" => {
            let portal = cfg.guide.portals.first().map(|p| p.name.clone()).unwrap_or_default();
            let mut p = PathBuf::from(format!("guide/{lang}"));
            if !portal.is_empty() {
                p.push(portal);
            }
            p.join(source::slug(fm_module.unwrap_or("general")))
                .join(format!("process-{}.md", source::slug(key)))
        }
        "doc" => PathBuf::from(if key == "index" {
            "index.md".to_string()
        } else {
            format!("{key}.md")
        }),
        _ => return Err(format!("write_doc does not take {kind} items")),
    })
}

fn write_doc(p: &mut Project, item: &str, markdown: &str) -> Result<String, String> {
    let (kind, lang, key) = split_item(item)?;
    let cfg = p.cfg.clone();
    check_lang(&cfg, lang)?;
    let mut sources = source::sources(&cfg)?;
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let mut fb = FactBuilder::new(&mut *p.api, &cfg, &labels);
    let pages = if matches!(kind, "guide" | "translation") {
        fb.pages()?
    } else {
        Vec::new()
    };
    let draft = Source::parse(Path::new("draft.md"), markdown)?;
    // the page's facts decide its module (a page in several menus), the same as facts' source_path
    let item_facts = match pages.iter().find(|x| x.key == key) {
        Some(pg) => Some(fb.facts(pg)?),
        None => None,
    };
    let module = draft
        .fm
        .module
        .clone()
        .or_else(|| item_facts.as_ref().map(|f| f.page.module.clone()));
    let rel = source_path_for(&cfg, &sources, kind, lang, key, module.as_deref(), &pages)?;
    let mut src = Source::parse(&rel, markdown)?;
    if !lang.is_empty() {
        src.fm.lang = lang.to_string();
    }
    if matches!(kind, "guide" | "translation") && pages.iter().any(|x| x.key == key) {
        src.fm.kind = Kind::Page;
        src.fm.routes = vec![key.to_string()];
        if src.fm.module.is_none() {
            src.fm.module = module.clone();
        }
    }
    if kind == "process" {
        src.fm.kind = Kind::Process;
    }
    if kind == "translation" {
        let primary = sources
            .iter()
            .find(|s| s.path == work::swap_lang(&rel, cfg.primary_language()))
            .ok_or("primary guide not found")?;
        src.fm.translated_from = Some(source::body_hash(&primary.body));
    }
    let page = if src.fm.kind == Kind::Page {
        pages.iter().find(|x| Some(&x.key) == src.fm.routes.first()).cloned()
    } else {
        None
    };
    let facts = if page.is_some() { item_facts } else { None };
    sources.retain(|s| s.path != rel);
    sources.push(src.clone());
    let problems = crate::validate::check_source(&src, facts.as_ref(), &cfg, &sources);
    let errors: Vec<&str> = problems
        .iter()
        .filter(|x| x.level == crate::validate::Level::Error)
        .map(|x| x.message.as_str())
        .collect();
    if !errors.is_empty() {
        return Ok(format!(
            "nothing was saved; fix these and send it again:\n- {}",
            errors.join("\n- ")
        ));
    }
    let full = crate::source::source_root(&cfg).join(&rel);
    std::fs::create_dir_all(full.parent().expect("has parent")).map_err(|e| e.to_string())?;
    std::fs::write(&full, src.render_text()).map_err(|e| format!("cannot write {}: {e}", full.display()))?;
    // an older copy of the same page guide elsewhere (module or portal folder changed) goes
    let mut moved = Vec::new();
    if src.fm.kind == Kind::Page {
        for old in sources.iter().filter(|s| {
            s.path != rel && s.fm.kind == Kind::Page && s.fm.lang == src.fm.lang && s.fm.routes == src.fm.routes
        }) {
            if std::fs::remove_file(crate::source::source_root(&cfg).join(&old.path)).is_ok() {
                moved.push(old.path.display().to_string());
            }
        }
    }
    let fp = match (&facts, &page) {
        (Some(f), Some(pg)) if src.fm.lang == cfg.primary_language() => {
            Some((pg.key.clone(), fingerprint(f, labels.files_hash())))
        }
        _ => None,
    };
    let st = State::update(&cfg, |st| {
        if let Some((key, fp)) = &fp {
            let e = st.pages.entry(key.clone()).or_default();
            e.fingerprint = fp.clone();
            e.documented_fingerprint = Some(fp.clone());
        }
        st.done(item);
        st.clone()
    })?;
    let r = crate::html::build(&cfg, &source::sources(&cfg)?, &st)?;
    let warnings: Vec<&str> = problems
        .iter()
        .filter(|x| x.level == crate::validate::Level::Warning)
        .map(|x| x.message.as_str())
        .collect();
    Ok(format!(
        "saved _source/{}; built {} files{}{}",
        rel.display(),
        r.written.len(),
        if moved.is_empty() {
            String::new()
        } else {
            format!("; removed the old copy {}", moved.join(", "))
        },
        if warnings.is_empty() {
            String::new()
        } else {
            format!("\nwarnings:\n- {}", warnings.join("\n- "))
        }
    ))
}

fn shoot(p: &mut Project, args: &Value) -> Result<Vec<Value>, String> {
    let st = State::load(&p.cfg);
    let spec: ShotSpec = match (args.get("spec"), s(args, "id")) {
        (Some(v), _) if v.is_object() => serde_json::from_value(v.clone()).map_err(|e| format!("bad spec: {e}"))?,
        (_, Some(id)) => st
            .shots
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no stored spec {id}; send a spec"))?,
        _ => return Err("send spec or id".into()),
    };
    let mut spec = spec;
    let eff = effective_cfg(p)?;
    spec.user = shot_user(&eff, &spec);
    crate::shots::validate_spec(&spec)?;
    crate::shots::resolve_path(&spec)?;
    check_lang(&p.cfg, &spec.lang)?;
    if p.shooter.is_none() {
        p.shooter = Some(Shooter::start(&eff)?);
    }
    let path = p.shooter.as_mut().expect("started").capture(&spec)?;
    State::update(&p.cfg, |st| {
        st.shots.insert(spec.id.clone(), spec.clone());
        let e = st.pages.entry(spec.page.clone()).or_default();
        if !e.shots.contains(&spec.id) {
            e.shots.push(spec.id.clone());
        }
        st.done(&format!("shot:{}", spec.id));
    })?;
    let png = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(vec![
        json!({"type": "text", "text": format!("saved {}; reference it as ![…]({})", path.display(), crate::shots::image_ref(&spec))}),
        json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(png), "mimeType": "image/png"}),
    ])
}

/// The endpoint or method behind a target, its action (for the test file) and its expected cases.
fn target_info(p: &mut Project, target: &str) -> Result<(String, Vec<cases::Case>, Value), String> {
    let cfg = p.cfg.clone();
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let mut fb = FactBuilder::new(&mut *p.api, &cfg, &labels);
    if let Some((method, uri)) = target
        .split_once(' ')
        .filter(|(m, _)| m.chars().all(|c| c.is_ascii_uppercase()))
    {
        let e = fb.endpoint(method, uri)?.ok_or_else(|| format!("no route {target}"))?;
        let info = json!({"endpoint": target, "action": e.action, "at": e.at, "middleware": e.middleware, "permissions": e.permissions, "rules": e.rules, "messages": e.messages});
        Ok((e.action.clone(), cases::for_endpoint(&e), info))
    } else {
        let (sym, src) = fb.symbol_source(target)?.ok_or_else(|| format!("no method {target}"))?;
        let info = json!({"method": target, "file": sym["file"], "line": sym["line"], "signature": sym["signature"], "source": src});
        Ok((target.to_string(), cases::for_method(target, &src), info))
    }
}

fn cases_text(p: &mut Project, target: &str) -> Result<String, String> {
    let (action, expected, info) = target_info(p, target)?;
    let cfg = p.cfg.clone();
    let endpoints = endpoint_for_ledger(target);
    let ledger = crate::ledger::scan(&cfg, &endpoints);
    let (file, class, ns) = testwrite::target_file(&cfg, target, &action);
    let list: Vec<Value> = expected
        .iter()
        .map(|c| {
            let by = cases::covered_by(&ledger, target, &c.case);
            json!({"case": c.case, "why": c.why, "covered_by": by.map(|b| format!("{} ({:?})", b.test, b.source))})
        })
        .collect();
    let existing: Vec<&crate::state::Covered> = ledger
        .iter()
        .filter(|c| c.target == target && c.source == CoverSource::Doctest)
        .collect();
    Ok(pretty(&json!({
        "target": target,
        "info": info,
        "cases": list,
        "test_file": file.strip_prefix(&cfg.backend).unwrap_or(&file),
        "test_class": class,
        "namespace": ns,
        "doctest_rows_already": existing.iter().map(|c| format!("{} {}", c.test, c.case)).collect::<Vec<_>>(),
        "how": "write_test with one parameterised method (e.g. test_<action>) and one row per missing case; args_php are the row's values in the method's parameter order. Use factories and actingAs for the user; positive and negative cases in the same provider.",
    })))
}

/// A bare endpoint for matching hand-written tests in the ledger.
fn endpoint_for_ledger(target: &str) -> Vec<crate::pages::Endpoint> {
    match target.split_once(' ') {
        Some((m, u)) if m.chars().all(|c| c.is_ascii_uppercase()) => vec![crate::pages::Endpoint {
            method: m.into(),
            uri: u.into(),
            ..Default::default()
        }],
        _ => Vec::new(),
    }
}

fn write_test(p: &mut Project, args: &Value) -> Result<String, String> {
    let target = s(args, "target").ok_or("target is required")?.to_string();
    let t = NewTests {
        method: s(args, "method").ok_or("method is required")?.to_string(),
        method_php: s(args, "method_php").map(String::from),
        rows: serde_json::from_value(args.get("rows").cloned().unwrap_or(json!([])))
            .map_err(|e| format!("bad rows: {e}"))?,
    };
    let remove: Vec<String> = args
        .get("remove")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let replace = args.get("replace").and_then(Value::as_bool).unwrap_or(false);
    let cfg = p.cfg.clone();
    if !remove.is_empty() {
        // removing needs no live route (it may be gone): the ledger knows the file of doctest's own rows
        let ledger = crate::ledger::scan(&cfg, &[]);
        let file = ledger
            .iter()
            .find(|c| {
                c.source == CoverSource::Doctest && c.target == target && c.test.ends_with(&format!("::{}", t.method))
            })
            .map(|c| cfg.backend.join(&c.file))
            .ok_or_else(|| format!("no doctest rows of {} for {target}", t.method))?;
        testwrite::remove_rows(&file, &t.method, &remove)?;
        if t.rows.is_empty() && !replace {
            State::update(&cfg, |st| st.done(&format!("tests:{target}")))?;
            return Ok(format!("removed rows {} from {}", remove.join(", "), t.method));
        }
    }
    let (action, _, _) = target_info(p, &target)?;
    let ledger = crate::ledger::scan(&cfg, &endpoint_for_ledger(&target));
    let covered: Vec<String> = t
        .rows
        .iter()
        .filter_map(|r| cases::covered_by(&ledger, &target, &r.case).map(|c| format!("{} (by {})", r.case, c.test)))
        .collect();
    if !covered.is_empty() {
        return Err(format!("already covered, not written: {}", covered.join(", ")));
    }
    let (file, class, ns) = testwrite::target_file(&cfg, &target, &action);
    // the whole insert → run → keep/roll back sequence holds the file, so a rollback never drops another worker's edit
    let _file_lock = testwrite::file_lock(&file)?;
    let before = std::fs::read_to_string(&file).ok();
    let restore = |why: String| -> String {
        let _ = match &before {
            Some(text) => std::fs::write(&file, text),
            None => std::fs::remove_file(&file),
        };
        format!("the test file was rolled back to how it was: {why}")
    };
    let mut t = t;
    if replace {
        let php = t.method_php.take().ok_or("replace needs method_php")?;
        testwrite::replace_method_unlocked(&file, &t.method, &php).map_err(restore)?;
    }
    if !t.rows.is_empty() {
        testwrite::insert_unlocked(&file, &class, &ns, &target, &t)
            .map_err(|e| if replace { restore(e) } else { e })?;
    }
    let rel = file
        .strip_prefix(&cfg.backend)
        .unwrap_or(&file)
        .to_string_lossy()
        .into_owned();
    let mut methods = vec![t.method.clone()];
    if t.rows.iter().any(|r| r.bug.is_some()) {
        methods.push(format!("{}_known_bugs", t.method));
    }
    let res = {
        let _lock = State::test_lock(&cfg)?;
        testwrite::run(&cfg, &testwrite::filter_for(&class, &methods))
    };
    let res = match res {
        Ok(r) => r,
        Err(e) => return Err(restore(e)),
    };
    let failed = res.failed_cases(&t.method);
    let bug_cases: Vec<String> = t
        .rows
        .iter()
        .filter(|r| r.bug.is_some())
        .map(|r| r.case.clone())
        .collect();
    if replace {
        let broken: Vec<&String> = failed.iter().filter(|c| !bug_cases.contains(c)).collect();
        if !broken.is_empty() {
            let detail: Vec<String> = res
                .failed
                .iter()
                .map(|(n, o, _)| format!("{n}: {}", testwrite::tail(o, 4)))
                .collect();
            return Err(restore(format!(
                "the new method_php fails rows {} — fix it and send it again:\n{}",
                broken.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
                detail.join("\n")
            )));
        }
    }
    let to_remove: Vec<String> = failed
        .iter()
        .filter(|c| !bug_cases.contains(c) && t.rows.iter().any(|r| &r.case == *c))
        .cloned()
        .collect();
    if !to_remove.is_empty() {
        testwrite::remove_rows_unlocked(&file, &t.method, &to_remove)?;
    }
    let passing_bugs: Vec<String> = res.passed_cases(&format!("{}_known_bugs", t.method));
    let method = t.method.clone();
    State::update(&cfg, |st| {
        st.failing.retain(|f| !testwrite::same_method(f, &method));
        st.done(&format!("tests:{target}"));
    })?;
    let mut out = format!(
        "{rel}: {} row(s) written{}",
        t.rows.len() - to_remove.len(),
        if replace { ", method replaced" } else { "" }
    );
    for (name, output, at) in &res.failed {
        if let Some(c) = to_remove.iter().find(|c| name.contains(&format!("\"{c}\""))) {
            out += &format!(
                "\n- {c} failed and was removed ({}): {}",
                at.as_deref().unwrap_or("?"),
                testwrite::tail(output, 8)
            );
        }
    }
    if !passing_bugs.is_empty() {
        out += &format!(
            "\n- marked as bugs but passing (not bugs; send them again without bug): {}",
            passing_bugs.join(", ")
        );
    }
    if !to_remove.is_empty() {
        out += "\nRead the code for each removed case: if the code is wrong, send the row again with bug set; if the test was wrong, fix args_php.";
    }
    Ok(out)
}

fn run_tests(p: &mut Project, filter: Option<&str>) -> Result<String, String> {
    let cfg = p.cfg.clone();
    let filter = match filter {
        Some(f) => f.to_string(),
        None => {
            if crate::ledger::scan(&cfg, &[])
                .iter()
                .all(|c| c.source != CoverSource::Doctest)
            {
                return Ok("doctest has written no tests yet".into());
            }
            filter_all(&cfg)
        }
    };
    let res = {
        let _lock = State::test_lock(&cfg)?;
        testwrite::run(&cfg, &filter)?
    };
    let failing: Vec<String> = res
        .failed
        .iter()
        .filter(|f| !f.0.contains("known bugs") && !f.0.contains("known_bugs"))
        .map(|f| f.0.clone())
        .collect();
    State::update(&cfg, |st| st.failing = failing)?;
    let points: Vec<String> = crate::issues::failing_tests(&res)
        .iter()
        .map(crate::issues::Issue::point)
        .collect();
    Ok(format!(
        "{} passed, {} failed{}",
        res.passed.len(),
        res.failed.len(),
        if points.is_empty() {
            String::new()
        } else {
            format!("\n- {}", points.join("\n- "))
        }
    ))
}

/// A --filter for every doctest method, per class.
fn filter_all(cfg: &Config) -> String {
    let mut by_class: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for c in crate::ledger::scan(cfg, &[]) {
        if c.source == CoverSource::Doctest
            && c.bug.is_none()
            && let Some((class, m)) = c.test.split_once("::")
        {
            by_class.entry(class.to_string()).or_default().insert(m.to_string());
        }
    }
    let parts: Vec<String> = by_class
        .iter()
        .map(|(c, ms)| format!("{c}::(?:{})", ms.iter().cloned().collect::<Vec<_>>().join("|")))
        .collect();
    format!("/(?:{})\\b/", parts.join("|"))
}

/// Serves MCP on stdio until stdin closes; tool calls run on `workers` threads, each with its own codemap clients
/// and Chrome, so several calls can run at the same time.
pub fn serve(workers: usize) {
    crate::update::check_in_background();
    let out: crate::progress::Shared = std::sync::Arc::new(std::sync::Mutex::new(Box::new(std::io::stdout())));
    let write = |out: &crate::progress::Shared, reply: Value| {
        let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(w, "{reply}");
        let _ = w.flush();
    };
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let mut handles = Vec::new();
    for _ in 0..workers.max(1) {
        let rx = rx.clone();
        let out = out.clone();
        handles.push(std::thread::spawn(move || {
            let mut server = Server::live_with(out.clone());
            loop {
                let msg = { rx.lock().unwrap_or_else(|e| e.into_inner()).recv() };
                let Ok(msg) = msg else { break };
                if let Some(reply) = server.handle(&msg) {
                    write(&out, reply);
                }
            }
        }));
    }
    let mut control = Server::live_with(out.clone());
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["method"] == "tools/call" {
            let _ = tx.send(message);
        } else if let Some(reply) = control.handle(&message) {
            write(&out, reply);
        }
    }
    drop(tx);
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tests::project;

    fn server() -> Server {
        Server::new(std::sync::Arc::new(
            |_cfg: &Config, _n: usize| -> Box<dyn CodemapApi + Send> { Box::new(crate::work::tests::api()) },
        ))
    }

    fn call(s: &mut Server, tool: &str, args: Value) -> (String, bool) {
        let reply = s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}})).unwrap();
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (text, reply["result"]["isError"].as_bool().unwrap_or(false))
    }

    #[test]
    fn initialize_and_list_tools() {
        let mut s = server();
        let init = s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "doctest");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("facts"));
        let list = s
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "status",
                "plan",
                "claim",
                "facts",
                "write_doc",
                "shoot",
                "cases",
                "write_test",
                "run_tests",
                "build",
                "check",
                "issues",
                "verify_changes",
                "finish",
                "report_problem",
                "doctor",
                "login"
            ]
        );
        assert!(
            s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
    }

    #[test]
    fn status_over_mcp() {
        let (_d, cfg) = project();
        let mut s = server();
        let (text, err) = call(&mut s, "status", json!({"project": cfg.root, "scope": "all"}));
        assert!(!err, "{text}");
        assert!(text.contains("guide:en:shop.orders.create"), "{text}");
    }

    #[test]
    fn status_sends_progress_when_asked() {
        let (_d, cfg) = project();
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let out: crate::progress::Shared = std::sync::Arc::new(std::sync::Mutex::new(Box::new(
            crate::progress::SharedVec(sink.clone()),
        )));
        let mut s = Server::with_output(
            std::sync::Arc::new(|_cfg: &Config, _n: usize| -> Box<dyn CodemapApi + Send> {
                Box::new(crate::work::tests::api())
            }),
            out,
        );
        let req = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "status", "arguments": {"project": cfg.root, "scope": "all"}, "_meta": {"progressToken": "p7"}}});
        let reply = s.handle(&req).unwrap();
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"progress\"")
        );
        let notes = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        assert!(
            notes.contains("notifications/progress") && notes.contains("\"p7\"") && notes.contains("[100%] plan ready"),
            "{notes}"
        );
    }

    #[test]
    fn facts_for_a_guide_item() {
        let (_d, cfg) = project();
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "facts",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create"}),
        );
        assert!(!err, "{text}");
        for want in [
            "Tenant",
            "Save",
            "guide/en/orders/shop-orders-create.md",
            "## Fields",
            "POST api/orders/{order}",
            "\"actions\"",
            "screenshot_plan",
            "## Edit",
            "What you should see",
        ] {
            assert!(text.contains(want), "{want} not in {text}");
        }
    }

    #[test]
    fn write_doc_rejects_unknown_name() {
        let (_d, cfg) = project();
        let mut s = server();
        let md =
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Submit**.\n";
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        assert!(!err);
        assert!(text.contains("nothing was saved") && text.contains("Submit"), "{text}");
        assert!(!cfg.out.join("_source/guide/en/orders/shop-orders-create.md").exists());
    }

    #[test]
    fn write_doc_saves_marks_documented_and_builds() {
        let (_d, cfg) = project();
        let mut s = server();
        let md =
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Save**.\n";
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        assert!(!err, "{text}");
        assert!(cfg.out.join("guide/en/orders/shop-orders-create.html").is_file());
        let st = State::load(&cfg);
        assert_eq!(
            st.pages["shop.orders.create"].documented_fingerprint.as_deref(),
            Some(st.pages["shop.orders.create"].fingerprint.as_str())
        );
        // the translation gets translated_from filled in
        let id_md = "---\ntitle: Pesanan Baru\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nIsi **Penyewa**, lalu klik **Save**.\n";
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "translation:id:shop.orders.create", "markdown": id_md}),
        );
        assert!(!err, "{text}");
        let saved = std::fs::read_to_string(cfg.out.join("_source/guide/id/orders/shop-orders-create.md"))
            .unwrap_or_else(|_| panic!("{text}"));
        assert!(
            saved.contains(&format!(
                "translated_from: {}",
                crate::source::body_hash("Click **Save**.\n")
            )),
            "{saved}"
        );
    }

    #[test]
    fn write_test_refuses_covered_case() {
        let (_d, cfg) = project();
        let mut s = server();
        let args = json!({
            "project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store",
            "method_php": "public function test_store(int $s): void {}",
            "rows": [{"case": "guest", "args_php": "401"}]
        });
        let (text, err) = call(&mut s, "write_test", args);
        assert!(err, "{text}");
        assert!(text.contains("guest") && text.contains("already covered"), "{text}");
    }

    #[test]
    fn write_test_keeps_passing_rows_and_drops_failing_ones() {
        let (_d, cfg) = project();
        std::fs::write(cfg.backend.join("fake.txt"), "  ✓ store with data set \"forbidden\"\n  ⨯ store with data set \"not_found\"\n   FAILED  Tests\\Feature\\Http\\Controllers\\OrderControllerTest > store with data set \"not_found\"\n  Expected 404 but received 200.\n\n  Tests: 1 failed, 1 passed\n").unwrap();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + "[tests]\nrun = \"cat fake.txt # {filter}\"\n";
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let mut s = server();
        let args = json!({
            "project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store",
            "method_php": "public function test_store(int $s): void {}",
            "rows": [{"case": "forbidden", "args_php": "403"}, {"case": "not_found", "args_php": "404"}]
        });
        let (text, err) = call(&mut s, "write_test", args);
        assert!(!err, "{text}");
        assert!(text.contains("not_found") && text.contains("removed"), "{text}");
        let php = std::fs::read_to_string(
            cfg.backend
                .join("tests/Feature/Http/Controllers/OrderControllerTest.php"),
        )
        .unwrap();
        assert!(
            php.contains("'forbidden' => [403]") && !php.contains("'not_found' => [404]"),
            "{php}"
        );
    }

    #[test]
    fn write_test_removes_rows() {
        let (_d, cfg) = project();
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let t = crate::testwrite::NewTests {
            method: "test_store".into(),
            method_php: Some("public function test_store(int $s): void {}".into()),
            rows: vec![
                crate::testwrite::Row {
                    case: "forbidden".into(),
                    args_php: "403".into(),
                    bug: None,
                },
                crate::testwrite::Row {
                    case: "invalid:note:required".into(),
                    args_php: "422".into(),
                    bug: None,
                },
            ],
        };
        crate::testwrite::insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            "POST api/orders/{order}",
            &t,
        )
        .unwrap();
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "write_test",
            json!({"project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store", "rows": [], "remove": ["invalid:note:required"]}),
        );
        assert!(!err, "{text}");
        let php = std::fs::read_to_string(&f).unwrap();
        assert!(
            php.contains("'forbidden'") && !php.contains("invalid:note:required"),
            "{php}"
        );
    }

    fn fake_runner(cfg: &Config, output: &str, fail: bool) {
        std::fs::write(cfg.backend.join("fake.txt"), output).unwrap();
        let run = if fail {
            "cat fake.txt; exit 255 # {filter}"
        } else {
            "cat fake.txt # {filter}"
        };
        let toml =
            std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap() + &format!("[tests]\nrun = \"{run}\"\n");
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
    }

    #[test]
    fn broken_php_is_rolled_back() {
        let (_d, cfg) = project();
        fake_runner(
            &cfg,
            "PHP Parse error: syntax error, unexpected '}' in OrderControllerTest.php on line 80\n",
            true,
        );
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let before = std::fs::read_to_string(&f).unwrap();
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "write_test",
            json!({"project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store",
            "method_php": "public function test_store(int $s): void { oops }", "rows": [{"case": "forbidden", "args_php": "403"}]}),
        );
        assert!(err, "{text}");
        assert!(text.contains("rolled back") && text.contains("Parse error"), "{text}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), before);
    }

    #[test]
    fn guide_item_is_always_a_page_guide() {
        let (_d, cfg) = project();
        let mut s = server();
        let md = "---\ntitle: New Order\n---\nClick **Submit**.\n";
        let (text, _) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        assert!(text.contains("nothing was saved") && text.contains("Submit"), "{text}");
    }

    #[test]
    fn unsafe_item_keys_are_refused() {
        let (_d, cfg) = project();
        let mut s = server();
        let md = "---\ntitle: X\n---\nx\n";
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "doc:../../README", "markdown": md}),
        );
        assert!(err && text.contains("doc:../../README"), "{text}");
        assert!(!cfg.backend.join("README.md").exists());
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:xx:shop.orders.create", "markdown": md}),
        );
        assert!(err && text.contains("xx"), "{text}");
        let (text, err) = call(
            &mut s,
            "shoot",
            json!({"project": cfg.root, "spec": {"id": "orders/x-1", "page": "p", "path": "/x/:id", "lang": "en"}}),
        );
        assert!(err && text.contains("needs params: id"), "{text}");
    }

    #[test]
    fn index_facts_include_publishing() {
        let (_d, cfg) = project();
        let mut s = server();
        let (text, err) = call(&mut s, "facts", json!({"project": cfg.root, "item": "doc:index"}));
        assert!(!err, "{text}");
        assert!(
            text.contains("/docs") && text.contains("document/guide") && text.contains("location"),
            "{text}"
        );
    }

    #[test]
    fn shot_user_defaults_to_the_portal_user() {
        let (_d, cfg) = project();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + "[guide.portals.shop]\npages=[\"shop.*\"]\nuser=\"buyer\"\n";
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let cfg = Config::find(&cfg.root).unwrap();
        let spec = ShotSpec {
            id: "orders/x-1".into(),
            page: "shop.orders.create".into(),
            path: "/shop/orders/create".into(),
            lang: "en".into(),
            ..ShotSpec::default()
        };
        assert_eq!(
            shot_user(&cfg, &spec).as_deref(),
            Some("shop"),
            "screenshots use their portal's session"
        );
        let mut own = spec.clone();
        own.user = Some("admin".into());
        assert_eq!(shot_user(&cfg, &own).as_deref(), Some("admin"));
    }

    #[test]
    fn rows_of_a_removed_route_can_be_removed() {
        let (_d, cfg) = project();
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let t = crate::testwrite::NewTests {
            method: "test_gone".into(),
            method_php: Some("public function test_gone(int $s): void {}".into()),
            rows: vec![crate::testwrite::Row {
                case: "ok".into(),
                args_php: "200".into(),
                bug: None,
            }],
        };
        crate::testwrite::insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            "GET api/gone",
            &t,
        )
        .unwrap();
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "write_test",
            json!({"project": cfg.root, "target": "GET api/gone", "method": "test_gone", "rows": [], "remove": ["ok"]}),
        );
        assert!(!err, "{text}");
        assert!(!std::fs::read_to_string(&f).unwrap().contains("test_gone"));
    }

    #[test]
    fn replace_that_breaks_existing_rows_is_rolled_back() {
        let (_d, cfg) = project();
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let t = crate::testwrite::NewTests {
            method: "test_store".into(),
            method_php: Some("public function test_store(int $s): void {}".into()),
            rows: vec![crate::testwrite::Row {
                case: "forbidden".into(),
                args_php: "403".into(),
                bug: None,
            }],
        };
        crate::testwrite::insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            "POST api/orders/{order}",
            &t,
        )
        .unwrap();
        let before = std::fs::read_to_string(&f).unwrap();
        fake_runner(
            &cfg,
            "  ⨯ store with data set \"forbidden\"\n   FAILED  Tests\\Feature\\Http\\Controllers\\OrderControllerTest > store with data set \"forbidden\"\n  ArgumentCountError\n\n  Tests: 1 failed\n",
            false,
        );
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "write_test",
            json!({"project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store", "replace": true, "rows": [],
            "method_php": "public function test_store(int $a, int $b): void {}"}),
        );
        assert!(
            err && text.contains("rolled back") && text.contains("forbidden"),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(&f).unwrap(), before);
    }

    #[test]
    fn rollback_keeps_another_workers_insert() {
        let (_d, cfg) = project();
        std::fs::write(cfg.backend.join("fake.txt"), "PHP Parse error: oops\n").unwrap();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + "[tests]\nrun = \"sleep 1; cat fake.txt; exit 255 # {filter}\"\n";
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        let root = cfg.root.clone();
        let a = std::thread::spawn(move || {
            let mut s = server();
            call(
                &mut s,
                "write_test",
                json!({"project": root, "target": "POST api/orders/{order}", "method": "test_store",
                "method_php": "public function test_store(int $s): void { oops }", "rows": [{"case": "forbidden", "args_php": "403"}]}),
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let b = crate::testwrite::NewTests {
            method: "test_other".into(),
            method_php: Some("public function test_other(int $s): void {}".into()),
            rows: vec![crate::testwrite::Row {
                case: "ok".into(),
                args_php: "200".into(),
                bug: None,
            }],
        };
        crate::testwrite::insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            "GET api/other",
            &b,
        )
        .unwrap();
        let (text, err) = a.join().unwrap();
        assert!(err && text.contains("rolled back"), "{text}");
        let php = std::fs::read_to_string(&f).unwrap();
        assert!(
            php.contains("function test_other(") && !php.contains("function test_store("),
            "{php}"
        );
    }

    #[test]
    fn rewriting_a_guide_at_a_new_path_removes_the_old_one() {
        let (_d, cfg) = project();
        let old = cfg.out.join("_source/guide/en/old-place/shop-orders-create.md");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "---\ntitle: New Order\nkind: page\nmodule: old-place\nroutes: [shop.orders.create]\n---\nClick **Save**.\n").unwrap();
        let mut s = server();
        let md = "---\ntitle: New Order\nkind: page\nroutes: [shop.orders.create]\n---\nClick **Save**.\n";
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        assert!(!err, "{text}");
        assert!(!old.exists(), "the old copy must go");
        assert!(
            cfg.out.join("_source/guide/en/orders/shop-orders-create.md").exists(),
            "{text}"
        );
    }

    #[test]
    fn ai_reports_a_problem() {
        let (_d, cfg) = project();
        let home = tempfile::tempdir().unwrap();
        let mut s = server();
        s.reports_root = home.path().to_path_buf();
        let (text, err) = call(
            &mut s,
            "report_problem",
            json!({"project": cfg.root, "kind": "wrong-answer", "step": "facts", "summary": "facts listed a button the page does not have", "target": "guide:en:shop.orders.create"}),
        );
        assert!(!err, "{text}");
        assert!(text.contains("doctest reports --send d_"), "{text}");
        let all = crate::reports::load_at(home.path());
        assert_eq!(
            (all.len(), all[0].source.as_str(), all[0].kind.as_str()),
            (1, "ai", "wrong-answer")
        );
    }

    #[test]
    fn internal_failures_are_recorded_automatically() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, cfg) = project();
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("codemap");
        std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = script.clone();
        let mut s = Server::new(std::sync::Arc::new(
            move |c: &Config, _n: usize| -> Box<dyn CodemapApi + Send> {
                Box::new(crate::codemap::LiveCodemap::with_binary(c, bin.clone()))
            },
        ));
        s.reports_root = home.path().to_path_buf();
        let (text, err) = call(&mut s, "status", json!({"project": cfg.root, "scope": "all"}));
        assert!(
            err && text.contains("codemap stopped") && text.contains("doctest reports --send d_"),
            "{text}"
        );
        let all = crate::reports::load_at(home.path());
        assert_eq!(
            (
                all.len(),
                all[0].kind.as_str(),
                all[0].source.as_str(),
                all[0].step.as_str()
            ),
            (1, "codemap-failure", "auto", "status")
        );
        let (text2, _) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "doc:../x", "markdown": "x"}),
        );
        assert!(
            !text2.contains("problem report"),
            "a wrong request is not doctest's fault: {text2}"
        );
        assert_eq!(crate::reports::load_at(home.path()).len(), 1);
    }

    #[test]
    fn doctor_over_mcp() {
        let (_d, cfg) = project();
        let mut s = server();
        let (text, err) = call(&mut s, "doctor", json!({"project": cfg.root}));
        assert!(!err, "{text}");
        for want in ["codemap", "git", "config", "chrome", "test login"] {
            assert!(text.to_lowercase().contains(want), "{want} missing from {text}");
        }
    }

    fn get_prompt(s: &mut Server, name: &str, args: Value) -> String {
        let r = s.handle(&json!({"jsonrpc": "2.0", "id": 9, "method": "prompts/get", "params": {"name": name, "arguments": args}})).unwrap();
        r["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_string()
    }

    #[test]
    fn prompts_are_offered_as_commands() {
        let mut s = server();
        let init = s
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
            .unwrap();
        assert!(init["result"]["capabilities"]["prompts"].is_object());
        let list = s
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "prompts/list"}))
            .unwrap();
        let names: Vec<&str> = list["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["all", "branch", "changes"]);
        let all = get_prompt(&mut s, "all", json!({}));
        for want in [
            "scope \"all\"",
            "1 worker",
            "facts",
            "screenshot_plan",
            "write_test",
            "finish",
        ] {
            assert!(all.contains(want), "{want} missing from the all prompt:\n{all}");
        }
        assert!(!all.contains("redo"), "redo only when asked");
        let redo = get_prompt(&mut s, "all", json!({"redo": "redo", "workers": "4"}));
        assert!(
            redo.contains("\"redo\": true") && redo.contains("4 workers") && redo.contains("claim"),
            "{redo}"
        );
        let branch = get_prompt(&mut s, "branch", json!({"base": "develop"}));
        assert!(
            branch.contains("scope \"branch\"") && branch.contains("\"base\": \"develop\""),
            "{branch}"
        );
        let bad = s
            .handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "prompts/get", "params": {"name": "nope"}}))
            .unwrap();
        assert!(bad["error"].is_object());
    }

    #[test]
    fn redo_lists_documented_pages_again() {
        let (_d, cfg) = project();
        let mut s = server();
        let md =
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Save**.\n";
        call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        let (plain, _) = call(&mut s, "plan", json!({"project": cfg.root, "scope": "all"}));
        assert!(!plain.contains("guide:en:shop.orders.create"), "{plain}");
        let (redo, err) = call(
            &mut s,
            "plan",
            json!({"project": cfg.root, "scope": "all", "redo": true}),
        );
        assert!(
            !err && redo.contains("guide:en:shop.orders.create") && redo.contains("redo requested"),
            "{redo}"
        );
    }

    #[test]
    fn finish_reports_how_long_the_run_took() {
        let (_d, cfg) = project();
        let mut s = server();
        call(&mut s, "status", json!({"project": cfg.root, "scope": "all"}));
        State::update(&cfg, |st| st.run_started = st.run_started.map(|t| t - 125)).unwrap();
        let (text, err) = call(&mut s, "finish", json!({"project": cfg.root}));
        assert!(!err && text.contains("took 2m 5s"), "{text}");
        let st = State::load(&cfg);
        assert_eq!(st.runs.last().unwrap().seconds, Some(125));
        assert!(st.run_started.is_none());
    }

    fn last_text(s: &mut Server, tool: &str, args: Value) -> (String, bool) {
        let reply = s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}})).unwrap();
        let parts = reply["result"]["content"].as_array().unwrap();
        (
            parts.last().unwrap()["text"].as_str().unwrap_or_default().to_string(),
            reply["result"]["isError"].as_bool().unwrap_or(false),
        )
    }

    #[test]
    fn every_step_ends_with_a_summary() {
        let (_d, cfg) = project();
        let mut s = server();
        let (sum, _) = last_text(&mut s, "status", json!({"project": cfg.root, "scope": "all"}));
        assert!(
            sum.contains("— doctest: status took") && sum.contains("found:") && sum.contains("next:"),
            "{sum}"
        );
        assert!(sum.contains("guide"), "{sum}");
        let bad = "---\ntitle: New Order\nkind: page\nroutes: [shop.orders.create]\n---\nClick **Submit**.\n";
        let (sum, _) = last_text(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": bad}),
        );
        assert!(
            sum.contains("found: 1 error") && sum.contains("Submit") && sum.contains("next: fix"),
            "{sum}"
        );
        let good = "---\ntitle: New Order\nkind: page\nroutes: [shop.orders.create]\n---\nClick **Save**.\n";
        let (sum, _) = last_text(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": good}),
        );
        assert!(
            sum.contains("found: nothing wrong") && sum.contains("next:") && sum.contains("claim"),
            "{sum}"
        );
        let (sum, err) = last_text(&mut s, "facts", json!({"project": cfg.root, "item": "guide:xx:nope"}));
        assert!(err && sum.contains("facts failed") && sum.contains("next:"), "{sum}");
        let (sum, _) = last_text(&mut s, "check", json!({"project": cfg.root}));
        assert!(sum.contains("found:") && sum.contains("error"), "{sum}");
    }

    #[test]
    fn finish_records_commits_and_changes_page() {
        let (_d, cfg) = project();
        let mut s = server();
        let (text, err) = call(&mut s, "finish", json!({"project": cfg.root, "note": "first pass"}));
        assert!(!err, "{text}");
        let st = State::load(&cfg);
        assert_eq!(st.last_commit.len(), 2);
        assert_eq!(st.runs.last().unwrap().note, "first pass");
        assert!(
            std::fs::read_to_string(cfg.out.join("system/changes.html"))
                .unwrap()
                .contains("first pass")
        );
    }

    #[test]
    fn two_workers_claim_disjoint_items_and_writing_releases() {
        let (_d, cfg) = project();
        let mut s = server();
        let (a, err) = call(
            &mut s,
            "claim",
            json!({"project": cfg.root, "worker": "w1", "count": 2, "scope": "all"}),
        );
        assert!(!err, "{a}");
        let (b, _) = call(
            &mut s,
            "claim",
            json!({"project": cfg.root, "worker": "w2", "count": 2}),
        );
        let ids = |t: &str| -> Vec<String> {
            serde_json::from_str::<Value>(t).unwrap()["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect()
        };
        let (ia, ib) = (ids(&a), ids(&b));
        assert_eq!(ia.len(), 2);
        assert!(ia.iter().all(|x| !ib.contains(x)), "{ia:?} {ib:?}");
        assert!(ia.contains(&"guide:en:shop.orders.create".to_string()), "{ia:?}");
        let md =
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Save**.\n";
        call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        let st = State::load(&cfg);
        assert!(!st.queue.contains(&"guide:en:shop.orders.create".to_string()));
        assert!(st.written.contains(&"guide:en:shop.orders.create".to_string()));
    }

    #[test]
    fn unknown_project_is_tool_error() {
        let mut s = server();
        let (text, err) = call(&mut s, "status", json!({"project": "/nonexistent/x"}));
        assert!(err && text.contains("/nonexistent/x"), "{text}");
    }
}
