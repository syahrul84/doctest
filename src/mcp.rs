//! MCP over stdio: the tools Claude uses to keep a project's guides, system document and tests current.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sha2::Digest;
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

const SHOOT_HELP: &str = "Take (or retake) a screenshot. Send a spec {id, page, path, params, user, lang, steps, crop, mask, allow_saves}, or an id alone to retake its stored spec. Returns the path to reference (and the image itself only with show: true). A shot stops after 90 s, a blank image is refused, and alert, confirm and leave-page dialogs are answered (confirm with Cancel).\nsteps (in order): {\"fill\": \"css\", \"value\": \"text\"} clears the field then types; {\"select\": \"css\", \"value\": \"v\"}; {\"click\": \"css\"}; {\"wait\": \"css\"} waits for an element; {\"wait_text\": \"Saved\"} waits for text; {\"scroll\": \"css\" | \"top\" | \"bottom\"}; {\"goto\": \"/path\"}.\nmask (on top of [screenshots] mask; inside shadow roots too, re-applied when the page re-renders): {\"selector\": \"css\", \"text\": \"Demo\"} replaces text or input values; {\"regex\": \"[A-Z]{2}-\\\\d+\", \"text\": \"AB-1\"} a JavaScript regex ((?i) at the start ignores case); {\"hide\": \"css\"} hides elements. The reply names masks that matched nothing.\ncrop: a CSS selector to capture just that element (scrolled into view first).\nSafe mode (on unless [screenshots] safe = false): the browser refuses POST/PUT/PATCH/DELETE while shooting, so steps cannot save or delete; the reply lists what it blocked. Set allow_saves only to show a real save result, on test data; retaking that spec saves again.";

pub const MCP_INSTRUCTIONS: &str = "Every doctest reply ends with a summary: how long the step took, what it found (errors, failures, counts) and what to do next — follow its next line.\n\ndoctest keeps a Laravel + Vue project's user guide (per page, per language), its system document (static HTML in document/, starting at index.html with server setup from scratch) and its PHPUnit tests current. You write; doctest finds what is missing or stale, gives you the facts, checks what you write, takes screenshots, runs tests and builds the HTML.\n\nWorkflow:\n1. status (scope changes = since the last finish; branch = what this branch changed since it left main/master, or base; all = whole system) and plan list work items: guide:<lang>:<page>, translation:<lang>:<page>, process:<lang>:<slug>, shots:<page>, shot:<id>, doc:<section>, tests:<target>.\n2. For each item call facts first and write only from them: the page's real labels, literal texts, endpoints, rules, messages, menu path. Never invent a field, button or message: write_doc refuses **bold** UI names the page does not have.\n3. Guides, text first: write the primary language first (page template: What this page is for, Before you start, one section per action from facts.actions (Add, Edit, Delete, Approve…) with numbered steps and what the user should see afterwards — the success message, the changed list, the validation errors — Fields table, Messages you may see, Related guides) WITHOUT screenshots: put a line <!-- shot: what it shows --> where each screenshot of facts.screenshot_plan belongs, and do not call shoot for guide items. Then translations (translation items; same structure, labels in that language, the same <!-- shot --> lines). When the guides are written, call shoot_pages once: doctest takes every page's opening screenshot itself, in parallel, and puts it in each language's guide. Last come shots:<page> items, for the action screenshots only: shoot a spec (path, params for :id, steps, crop, user, lang) and replace the matching <!-- shot --> line with ![alt](guide/img/<lang>/<id>.webp) in the primary guide and in every translation (translations reuse the primary language's images unless facts says otherwise). Mask real data with [screenshots] mask.\n4. Portals: the app's sign-in areas (admin, selfcare, a Filament panel, a member portal…), found from its login pages; each has its own guide and its own sign-in. facts gives the page's source_path inside its portal; screenshots use the portal's session (public pages none); when shoot says a portal is not signed in, call login with that portal. The guide folder is published alone at the publish path, so guide pages never link to the start page or system document, and images live in guide/img/<lang>/.\n5. Server setup (doc:index): from facts only (versions, extensions, env keys, services, deploy files) for a real server, not localhost; mark any step the repo does not back with a line <!-- unverified --> before it.\n6. Tests: cases lists the expected cases of a target and which test covers each. write_test adds ONE parameterised test method per target with a data provider: rows keyed by case (ok, guest, forbidden, not_found, invalid:<field>:<rule>, boundary:<field>:min|max, filter:<param>, state:<name>, throws:<Exception>, returns:<name>), covering positive and negative cases; later calls add rows (leave method_php out). A case already covered is refused. doctest writes the data provider itself (method_php is the test method alone). Failing rows are removed and reported with PHPUnit's message; after reading the code, if the code is wrong, send the row again with bug set: it is kept in group doctest-bug for the developer. A target PHPUnit cannot test (exit(), dd(), real network, a package's route): write_test with skip: \"why\".\nIf doctest itself gives a wrong or confusing answer, call report_problem; failures inside doctest are recorded automatically. A bug in the app you are documenting (a page that errors, a form that cannot save) is not a doctest problem: tell the person, and in tests keep the failing row with bug set. Either way tell the person the report id and that `doctest reports --send <id>` files it as a GitHub issue. Run doctor when something about the setup fails.\n7. issues lists code to fix as points (file:line, reason, fix): N+1, missing indexes, unoptimised queries and Vue code, failing tests. verify_changes validates the current change (affected tests, doctest tests, problems on changed lines, what is left). Tell the user about the points.\n8. Parallel work: to go faster, start several workers (e.g. 4 subagents); each calls claim with its own worker name to get its own items, works them, and claims again until none are left. Call doctest's tools directly, never through scripts that start a new doctest per call: one doctest keeps codemap answers and page facts between calls, a new one starts cold (and a new Chrome per screenshot). Long guides can be written to a file and sent with write_doc markdown_file; facts brief: true gives a shorter reply. Tests: send several targets in one write_test call (tests: [...]): they share one test run, which is most of a write_test's time. Test runs go one at a time unless [tests] parallel gives each its own database. Long calls report progress (percent).\n9. check, then finish (records the commits so the next changes scope starts here).";

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
            json!({"scope": scope, "base": base, "limit": {"type": "integer"}, "redo": {"type": "boolean", "description": "list every page guide again (rewrite all), not only missing or stale ones"}, "portals": {"type": "array", "items": {"type": "string"}, "description": "only these portals' items, e.g. [\"admin\",\"selfcare\"] (items of no portal, like the system document, stay)"}, "queue": {"type": "string", "enum": ["guides", "tests"], "description": "guides (guides, translations, screenshots, system document) or tests"}}),
            &[]
        ),
        tool(
            "plan",
            "The ordered work items (id, kind, reason). Work through them with facts → write_doc / shoot / write_test.",
            json!({"scope": scope, "base": base, "limit": {"type": "integer"}, "redo": {"type": "boolean", "description": "list every page guide again (rewrite all), not only missing or stale ones"}, "portals": {"type": "array", "items": {"type": "string"}, "description": "only these portals' items, e.g. [\"admin\",\"selfcare\"] (items of no portal, like the system document, stay)"}, "queue": {"type": "string", "enum": ["guides", "tests"], "description": "guides (guides, translations, screenshots, system document) or tests"}}),
            &[]
        ),
        tool(
            "claim",
            "For parallel AI workers: hands this worker its own next work items (not claimed by others; claims expire after an hour; renew: true keeps this worker's items another hour), only of its queue, kinds and portals when given. Claiming again hands back the items this worker still holds (unwritten) to the end of the queue, so it gets new ones. Refreshes the queue from a plan when empty or refresh is true.",
            json!({"worker": {"type": "string"}, "count": {"type": "integer"}, "renew": {"type": "boolean"}, "kinds": {"type": "array", "items": {"type": "string"}, "description": "item kinds: guide, translation, process, shot, doc, tests"}, "refresh": {"type": "boolean"}, "redo": {"type": "boolean"}, "scope": scope, "base": base, "portals": {"type": "array", "items": {"type": "string"}, "description": "only these portals' items, e.g. [\"admin\",\"selfcare\"] (items of no portal, like the system document, stay)"}, "queue": {"type": "string", "enum": ["guides", "tests"], "description": "guides (guides, translations, screenshots, system document) or tests"}}),
            &["worker"]
        ),
        tool(
            "facts",
            "Everything needed for one work item: page labels/literals/endpoints/rules/messages/menu path, current and primary text, a skeleton, the source path; server facts for doc:index; cases for tests:<target>. brief: true leaves out the current guide text (read it at source_path), component lists and long notes.",
            json!({"item": {"type": "string"}, "brief": {"type": "boolean"}}),
            &["item"]
        ),
        tool(
            "write_doc",
            "Store a guide, translation, process guide or system section (Markdown with a --- front matter header). Refused with the problems when it names UI the page lacks or links break; else saved and the HTML rebuilt. markdown_file: a file holding the Markdown instead of markdown (write it with your file tool, then send its path).",
            json!({"item": {"type": "string"}, "markdown": {"type": "string"}, "markdown_file": {"type": "string", "description": "path of a file holding the Markdown, instead of markdown"}}),
            &["item"]
        ),
        tool(
            "shoot_pages",
            "Text first, then this: takes every written page guide's opening screenshot (the page as it opens, no steps) itself, several browsers at once, and puts it in the primary guide and every translation, at the <!-- shot: the page as it opens --> line or after the first paragraph. Pages whose path needs an :id, and pages that already show a screenshot, are left for their shots:<page> item. Long: reports progress.",
            json!({"portals": {"type": "array", "items": {"type": "string"}, "description": "only these portals' pages"}, "limit": {"type": "integer", "description": "at most this many pages (to try it on a few first)"}, "retake": {"type": "boolean", "description": "shoot again even where the image file exists"}}),
            &[]
        ),
        tool(
            "retake",
            "Take stored screenshots again, several browsers at once, after a [screenshots] change (mask, colour scheme, width) or app change: all: true, pages: [route patterns], or ids: [...]. A shot whose element appears late is tried twice. Long: reports progress.",
            json!({"all": {"type": "boolean"}, "pages": {"type": "array", "items": {"type": "string"}}, "ids": {"type": "array", "items": {"type": "string"}}, "workers": {"type": "integer"}}),
            &[]
        ),
        tool(
            "shoot",
            SHOOT_HELP,
            json!({"spec": {"type": "object", "description": "id, page, path, params, user, lang, steps, crop, mask, allow_saves (the fields may also be sent at the top level)"}, "id": {"type": "string", "description": "alone: retake the stored spec of this id"}, "retake_page": {"type": "string", "description": "with lang: retake every stored screenshot of this page in that language ([screenshots] per_language)"}, "lang": {"type": "string"}, "show": {"type": "boolean", "description": "send the image back to look at it (off by default: the reply already says when a shot is blank, on the sign-in page, blocked a save or masked nothing)"}}),
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
            "Add data-provider rows (and the parameterised method the first time) for a target, run them (only its test file), keep the rows whose own data set passed, remove failing or not-run ones with PHPUnit's message (unless bug is set). doctest writes the data provider cases_<method> itself: method_php is the test method alone, without a DataProvider attribute or provider method. A row sent again with bug added or taken away moves between the main and bug providers. A target with only bug rows keeps its method as a helper of <method>_known_bugs. Several targets at once: tests: [{target, method, method_php, rows, ...}, ...] writes them all and runs them in ONE test run (much faster than one call each). skip: \"why\" marks a target PHPUnit cannot test (exit(), dd(), real network, a package's route) so status and claim stop listing it; unskip: true undoes it.",
            json!({"tests": {"type": "array", "items": {"type": "object"}, "description": "several targets in one call and one test run; each item takes the same fields as a single call"}, "target": {"type": "string"}, "skip": {"type": "string"}, "unskip": {"type": "boolean"}, "method": {"type": "string"}, "method_php": {"type": "string"}, "rows": {"type": "array", "items": {"type": "object", "properties": {"case": {"type": "string"}, "args_php": {"type": "string"}, "bug": {"type": "string"}}, "required": ["case", "args_php"]}}, "remove": {"type": "array", "items": {"type": "string"}, "description": "case keys of obsolete rows to delete"}, "replace": {"type": "boolean", "description": "replace the existing doctest method with method_php (fix a broken body)"}}),
            &[]
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
            "Check guides against pages, links, images, env keys, failing doctest tests; lists known-bug tests and code points. Narrow it with changed: true (guides git sees changed) or only: [route keys or guide paths, * allowed]; a narrowed check skips the project-wide parts.",
            json!({"changed": {"type": "boolean"}, "only": {"type": "array", "items": {"type": "string"}}}),
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
        let strings = |k: &str| -> Vec<String> {
            args.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        let portals = strings("portals");
        // kinds narrow a queue (both given: only the kinds in both); "shot" and "shots" both mean screenshot items
        let kinds = work::wanted_kinds(&strings("kinds"), &work::queue_kinds(s(args, "queue").unwrap_or(""))?);
        match name {
            "status" | "plan" => {
                let mut st = State::load(&p.cfg);
                let mut plan = run_plan(&factory, p, &mut st, &scope, progress)?;
                if args.get("redo").and_then(Value::as_bool).unwrap_or(false) {
                    work::add_redo(&mut plan, &p.cfg, &st);
                }
                if !portals.is_empty() || !kinds.is_empty() {
                    let ip = work::item_portals(&plan, &st, &p.cfg);
                    plan.items.retain(|i| work::item_wanted(&i.id, &kinds, &portals, &ip));
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
                let redo = args.get("redo").and_then(Value::as_bool).unwrap_or(false);
                if args.get("renew").and_then(Value::as_bool).unwrap_or(false) {
                    let held = State::update(&p.cfg, |s| s.renew_claims(&worker, crate::state::now()))?;
                    return text(pretty(
                        &json!({"worker": worker, "renewed": held, "note": "these items stay yours for another hour"}),
                    ));
                }
                // the whole plan is queued; each worker takes only what its queue, kinds and portals allow
                let rebuild = |p: &mut Project| -> Result<(), String> {
                    let mut st = State::load(&p.cfg);
                    let mut plan = run_plan(&factory, p, &mut st, &scope, progress)?;
                    if redo {
                        work::add_redo(&mut plan, &p.cfg, &st);
                    }
                    let ip = work::item_portals(&plan, &st, &p.cfg);
                    State::update(&p.cfg, |s| {
                        s.queue = plan.items.iter().map(|i| i.id.clone()).collect();
                        s.reasons = plan.items.iter().map(|i| (i.id.clone(), i.reason.clone())).collect();
                        s.item_portals = ip.clone();
                        let q = s.queue.clone();
                        s.claims.retain(|k, _| q.contains(k));
                    })
                };
                let take = |p: &mut Project| {
                    State::update(&p.cfg, |s| {
                        let ip = s.item_portals.clone();
                        // text first: a page's screenshot item waits until its own guide and translations are written
                        let writing: BTreeSet<String> = s
                            .queue
                            .iter()
                            .filter(|id| id.starts_with("guide:") || id.starts_with("translation:"))
                            .filter_map(|id| id.splitn(3, ':').nth(2).map(String::from))
                            .collect();
                        let wanted = |id: &str| {
                            work::item_wanted(id, &kinds, &portals, &ip)
                                && !id.strip_prefix("shots:").is_some_and(|k| writing.contains(k))
                        };
                        let got: Vec<(String, String)> = s
                            .claim_where(&worker, count, crate::state::now(), &wanted)
                            .into_iter()
                            .map(|id| {
                                let r = s.reasons.get(&id).cloned().unwrap_or_default();
                                (id, r)
                            })
                            .collect();
                        let left = s.queue.iter().filter(|id| wanted(id)).count();
                        (got, left)
                    })
                };
                let refreshed = args.get("refresh").and_then(Value::as_bool).unwrap_or(false)
                    || State::load(&p.cfg).queue.is_empty();
                if refreshed {
                    rebuild(p)?;
                }
                let (mut got, mut left) = take(p)?;
                // nothing left for this filter in the queue: items written since it was built (new translations) may
                // have made new work, so build it again once before saying so
                if got.is_empty() && !refreshed {
                    rebuild(p)?;
                    (got, left) = take(p)?;
                }
                let note = if got.iter().any(|(id, _)| id.starts_with("shots:")) {
                    "screenshot items: if shoot_pages has not run since the guides were written, call it once first (it takes every page's opening shot); then per item take only the shots still missing (facts → shoot → write_doc for each language); claims expire after an hour (claim with renew: true keeps them); claiming again hands back what you did not write"
                } else {
                    "work these items (facts → write_doc / write_test; guides are written without screenshots); a written item leaves the queue; a page's screenshot item comes once its guide and translations are written; claims expire after an hour (claim with renew: true keeps them); claiming again hands back what you did not write, to the end of the queue"
                };
                text(pretty(
                    &json!({"worker": worker, "items": got.iter().map(|(id, r)| json!({"id": id, "reason": r})).collect::<Vec<_>>(), "queued": left, "note": note}),
                ))
            }
            "facts" => {
                let full = facts(p, s(args, "item").ok_or("item is required")?)?;
                if args.get("brief").and_then(Value::as_bool).unwrap_or(false) {
                    text(brief_facts(&full))
                } else {
                    text(full)
                }
            }
            "write_doc" => {
                // the guide as text, or from a file the worker wrote (no need to send long markdown in the call)
                let markdown = match (s(args, "markdown"), s(args, "markdown_file")) {
                    (Some(m), _) => m.to_string(),
                    (None, Some(f)) => std::fs::read_to_string(f).map_err(|e| format!("markdown_file {f}: {e}"))?,
                    (None, None) => return Err("markdown (or markdown_file) is required".into()),
                };
                text(write_doc(p, s(args, "item").ok_or("item is required")?, &markdown)?)
            }
            "shoot" => shoot(p, args),
            "shoot_pages" => text(shoot_pages(p, args, progress)?),
            "retake" => text(retake(p, args, progress)?),
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
                let only = crate::check::Only {
                    changed: args.get("changed").and_then(Value::as_bool).unwrap_or(false),
                    names: strings("only"),
                };
                let r = crate::check::run_only(&p.cfg, &mut *p.api, &mut st, progress, &only)?;
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
                "shoot" => {
                    let blocked = text.contains("safe mode blocked");
                    let masks = text.contains("masks that matched nothing");
                    let mut found = Vec::new();
                    if blocked {
                        found.push("safe mode blocked a save");
                    }
                    if masks {
                        found.push("some masks matched nothing");
                    }
                    if text.contains("saved again") {
                        found.push("a retake repeated a save");
                    }
                    (
                        if found.is_empty() {
                            "nothing wrong".into()
                        } else {
                            found.join("; ")
                        },
                        if found.is_empty() {
                            "put the image in the guide with the path above, then continue".into()
                        } else {
                            "read the notes above; retake if the image is wrong, else put it in the guide".into()
                        },
                    )
                }
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

/// Facts for a module section: its pages (with their guides), the endpoints its pages call (with controller,
/// permissions and middleware), the controllers, the tables of the models they use, the jobs they dispatch and the
/// menu permissions.
fn module_facts(
    fb: &mut FactBuilder,
    cfg: &Config,
    labels: &Labels,
    sources: &[Source],
    primary: &str,
    m: &str,
) -> Result<Value, String> {
    use std::collections::{BTreeMap, BTreeSet};
    static MODEL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^use\s+App\\Models\\([\w\\]+)\s*;").unwrap());
    static JOB: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?:dispatch(?:Now|Sync)?\(\s*new\s+|new\s+)(\w+Job)\b|\b(\w+)::dispatch(?:Now|Sync|AfterResponse)?\(",
        )
        .unwrap()
    });
    let pages: Vec<crate::pages::Page> = fb
        .pages()?
        .into_iter()
        .filter(|x| source::slug(&x.module) == m)
        .collect();
    let mut cache = crate::factcache::FactCache::load(cfg, labels.files_hash());
    let mut list = Vec::new();
    let mut endpoints: BTreeMap<String, Value> = BTreeMap::new();
    let mut controller_files: BTreeSet<String> = BTreeSet::new();
    let mut permissions: BTreeSet<String> = BTreeSet::new();
    for page in &pages {
        let f = match cache.get(cfg, &page.key).filter(|f| f.page == *page) {
            Some(f) => f,
            None => {
                let f = fb.facts(page)?;
                cache.put(cfg, &f);
                f
            }
        };
        let g = work::page_guide(sources, &page.key, primary);
        list.push(json!({"page": page.key, "path": page.path, "portal": page.portal, "guide": g.map(|g| g.path.clone()), "summary": g.map(|g| g.fm.summary.clone())}));
        for e in &f.endpoints {
            let id = format!("{} {}", e.method, e.uri);
            let entry = endpoints.entry(id).or_insert_with(|| {
                json!({"action": e.action, "at": e.at, "permissions": e.permissions, "middleware": e.middleware, "fields": e.rules.keys().collect::<Vec<_>>(), "pages": [], "possible": true})
            });
            if let Some(a) = entry["pages"].as_array_mut() {
                a.push(json!(page.key));
            }
            if !e.possible {
                entry["possible"] = json!(false);
            }
            permissions.extend(e.permissions.iter().cloned());
            if let Some((file, _)) = e.at.as_deref().and_then(|a| a.rsplit_once(':')) {
                controller_files.insert(file.to_string());
            }
        }
    }
    cache.save();
    for entry in fb
        .menu
        .iter()
        .filter(|x| source::slug(&x.module) == m && !x.aro.is_empty())
    {
        permissions.insert(entry.aro.clone());
    }
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut jobs: BTreeSet<String> = BTreeSet::new();
    for file in &controller_files {
        let src = std::fs::read_to_string(cfg.backend.join(file)).unwrap_or_default();
        models.extend(MODEL.captures_iter(&src).map(|c| c[1].to_string()));
        for c in JOB.captures_iter(&src) {
            if let Some(j) = c.get(1).or(c.get(2)) {
                let j = j.as_str();
                if j.ends_with("Job") || !["Bus", "Event", "Queue", "self", "static", "parent"].contains(&j) {
                    jobs.insert(j.to_string());
                }
            }
        }
    }
    let mut tables: Vec<Value> = Vec::new();
    for model in models.iter().take(40) {
        let sym = fb
            .api()
            .call(BACKEND, "get_symbol", json!({"name": format!("App\\Models\\{model}")}))?;
        if let Some(table) = sym.data["model"]["table"].as_str() {
            tables.push(json!({"model": model, "table": table, "relations": sym.data["model"]["relations"]}));
        }
    }
    let n = endpoints.len();
    Ok(json!({
        "pages": list,
        "endpoints": endpoints,
        "controllers": controller_files,
        "tables": tables,
        "jobs": jobs,
        "permissions": permissions,
        "note": format!("{} page(s), {n} endpoint(s); possible: true marks one reached only through shared code (it may belong to another module). Describe what the module does, its screens, its API (method, URI, permission), the tables it keeps its data in and the jobs it runs; get_schema in codemap gives the columns of a table.", pages.len()),
    }))
}

/// A Laravel app's background work read from its files: queued job classes (app/Jobs, ShouldQueue), artisan
/// commands (app/Console/Commands: signature and description) and the schedule lines (routes/console.php,
/// app/Console/Kernel.php).
fn background_work(backend: &Path) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    fn php_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                php_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "php") {
                out.push(p);
            }
        }
    }
    let class = regex::Regex::new(r"(?m)^\s*(?:final\s+|abstract\s+)?class\s+(\w+)").expect("regex");
    let field = |name: &str| {
        regex::Regex::new(&format!(
            r#"(?s)\$\s*{name}\s*=\s*(?:'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)")"#
        ))
        .expect("regex")
    };
    let (signature, description) = (field("signature"), field("description"));
    let rel = |p: &Path| p.strip_prefix(backend).unwrap_or(p).display().to_string();
    let mut files = Vec::new();
    php_files(&backend.join("app/Jobs"), &mut files);
    files.sort();
    let jobs: Vec<Value> = files
        .iter()
        .filter_map(|f| {
            let t = std::fs::read_to_string(f).ok()?;
            let c = class.captures(&t)?;
            Some(json!({"class": &c[1], "file": rel(f), "queued": t.contains("ShouldQueue")}))
        })
        .collect();
    let mut files = Vec::new();
    php_files(&backend.join("app/Console/Commands"), &mut files);
    files.sort();
    let text_of = |c: Option<regex::Captures>| {
        c.and_then(|c| {
            c.get(1)
                .or(c.get(2))
                .map(|m| m.as_str().split_whitespace().collect::<Vec<_>>().join(" "))
        })
    };
    let commands: Vec<Value> = files
        .iter()
        .filter_map(|f| {
            let t = std::fs::read_to_string(f).ok()?;
            let c = class.captures(&t)?;
            Some(json!({
                "class": &c[1],
                "file": rel(f),
                "signature": text_of(signature.captures(&t)),
                "description": text_of(description.captures(&t)),
            }))
        })
        .collect();
    let mut schedule = Vec::new();
    for f in ["routes/console.php", "app/Console/Kernel.php", "bootstrap/app.php"] {
        let Ok(t) = std::fs::read_to_string(backend.join(f)) else {
            continue;
        };
        for (i, line) in t.lines().enumerate() {
            let l = line.trim();
            if (l.contains("->command(") || l.contains("->job(") || l.contains("Schedule::") || l.contains("->call("))
                && !l.starts_with("//")
            {
                schedule.push(format!("{f}:{}: {l}", i + 1));
            }
        }
    }
    (jobs, commands, schedule)
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
        "shot" | "tests" | "shots" => Ok((kind, "", rest)),
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

fn skeleton(f: &crate::pages::PageFacts) -> String {
    let title = f.menu_titles.last().cloned().unwrap_or_else(|| f.page.key.clone());
    let mut md = format!(
        "---\ntitle: {title}\nkind: page\nmodule: {}\nroutes: [{}]\norder: 10\nsummary: TODO one line\n---\n# {title}\n\nTODO what this page is for.\n\n<!-- shot: the page as it opens -->\n\n## Before you start\n\n## Step by step\n1. Open {}.\n\n## Fields\n\n| Field | What to enter | Required |\n| --- | --- | --- |\n",
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
            "\n## {} …\n\n1. TODO steps (bold only on-screen names).\n\n**What you should see:** {}\n\n<!-- shot: {}: the form or dialog -->\n",
            a.name,
            if a.success.is_empty() {
                "TODO the result on screen".to_string()
            } else {
                a.success.iter().map(|m| format!("*{m}*")).collect::<Vec<_>>().join(" ")
            },
            a.name,
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
                .ok_or_else(|| no_page(&cfg, item, key))?;
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
            if kind == "translation" {
                let per_language = cfg.screenshots.per_language;
                return Ok(pretty(&json!({
                    "item": item,
                    "source_path": path,
                    "page": {"key": page.key, "path": page.path, "module": page.module, "menu_path": f.menu_titles},
                    "labels": f.labels.get(lang),
                    "literal_texts": f.literals,
                    "current_text": current,
                    "primary_text": primary_text,
                    "screenshots": if per_language {
                        format!("this project takes screenshots per language: call shoot with {{\"retake_page\": \"{key}\", \"lang\": \"{lang}\"}} once to retake every stored screenshot of this page in {lang}, then use guide/img/{lang}/… in place of guide/img/{primary}/…")
                    } else {
                        format!("reuse the {primary} guide's screenshots: keep its ![…](guide/img/{primary}/…) lines as they are (translate only the alt text) and take no new ones")
                    },
                    "rules": "Same structure and sections as primary_text; translate the text, and write on-screen names in bold as they read in this language (from labels or literal_texts). Keep message texts exact in italics.",
                })));
            }
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
                "skeleton": if kind == "guide" && current.is_none() { Some(skeleton(&f)) } else { None },
                "actions": crate::pages::actions(&f),
                "possible_actions": crate::pages::possible_actions(&f),
                "possible_actions_note": "reached only through shared code (a shared screen, a layout or an API module) and no matching button was found on the page: document one only if you see it on the screen; they are not required",
                "screenshot_plan": crate::pages::screenshot_plan(&crate::pages::required_actions(&f)),
                "optional_screenshots": crate::pages::optional_shots(&crate::pages::required_actions(&f)),
                "screenshot_plan_without_data": crate::pages::screenshot_plan_for(&crate::pages::required_actions(&f), true),
                "min_screenshots": cfg.guide.min_screenshots.unwrap_or(crate::pages::screenshot_plan(&crate::pages::required_actions(&f)).len()),
                "screenshots_note": "Text first: write the guide now WITHOUT screenshots, leaving a line <!-- shot: what it shows --> where each screenshot in screenshot_plan belongs (the skeleton has them). Do not call shoot for this item. Later shoot_pages takes every page's opening shot itself and puts it at <!-- shot: the page as it opens -->, and a shots:<page> item asks for the action shots. If the page has no records to show, put <!-- no-data: why --> in the guide: then only screenshot_plan_without_data is needed.",
                "screenshots": shots,
                "image_ref_format": format!("guide/img/{lang}/{}/{}-<n>.webp", source::slug(&page.module), source::slug(&page.key)),
                "rules": "Bold (**...**) only on-screen names from labels, literal_texts or menu_path. Exact on-screen message texts in italics. One section per action (heading naming it) with numbered steps and 'What you should see' after Add, Edit, Delete and every other action, including the validation errors for required fields. No screenshots yet: a <!-- shot: … --> line where each one goes.",
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
                &json!({"item": item, "how": if key == "suggest" { format!("pick the processes worth a guide from the menu groups (a task that crosses several pages) and write each with write_doc item process:{lang}:<slug>; writing any one completes this suggestion") } else { "write the process guide with write_doc for this item".to_string() }, "source_path_hint": format!("guide/{lang}/<module>/process-{key}.md"), "front_matter": "kind: process, routes: [the pages in order], covers: [other pages it touches]", "menu": menu, "page_guides": guides}),
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
                let mut d = fb.api().call(BACKEND, "get_schema", json!({"background": true}))?.data;
                // the queued jobs, artisan commands and schedule straight from the app's folders, in case the index
                // above lists only some of them
                let (jobs, commands, schedule) = background_work(&cfg.backend);
                if let Some(o) = d.as_object_mut() {
                    o.insert("queued_jobs".into(), json!(jobs));
                    o.insert("commands".into(), json!(commands));
                    o.insert("schedule".into(), json!(schedule));
                } else {
                    d = json!({"index": d, "queued_jobs": jobs, "commands": commands, "schedule": schedule});
                }
                d
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
                module_facts(&mut fb, &cfg, &labels, &sources, &primary, m)?
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
            // the server block (versions, extensions, env keys, services) only where it is written about
            let server = if ["index", "system/configuration", "system/architecture"].contains(&key) {
                serde_json::to_value(&server).unwrap_or(Value::Null)
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
        "shots" => {
            let pages = fb.pages()?;
            let page = pages
                .iter()
                .find(|x| x.key == key)
                .ok_or_else(|| no_page(&cfg, item, key))?;
            let f = fb.facts(page)?;
            let acts = crate::pages::actions(&f);
            let st = State::load(&cfg);
            let shots: Vec<&ShotSpec> = st.shots.values().filter(|s| s.page == key).collect();
            let marker = regex::Regex::new(r"<!--\s*shot:\s*([^>]*?)\s*-->").expect("regex");
            let guides: Vec<Value> = cfg
                .languages
                .iter()
                .filter_map(|lang| {
                    let g = work::page_guide(&sources, key, lang)?;
                    let item = if *lang == primary {
                        format!("guide:{lang}:{key}")
                    } else {
                        format!("translation:{lang}:{key}")
                    };
                    Some(json!({
                        "lang": lang,
                        "write_doc_item": item,
                        "images": work::guide_images(&g.body),
                        "places_left": marker.captures_iter(&g.body).map(|c| c[1].to_string()).collect::<Vec<_>>(),
                        "text": g.render_text(),
                    }))
                })
                .collect();
            Ok(pretty(&json!({
                "item": item,
                "page": {"key": f.page.key, "path": f.page.path, "portal": f.page.portal, "module": f.page.module},
                "missing": work::shots_short(&cfg, &sources, key, &f),
                "screenshot_plan": crate::pages::screenshot_plan(&crate::pages::required_actions(&f)),
                "screenshot_plan_without_data": crate::pages::screenshot_plan_for(&crate::pages::required_actions(&f), true),
                "actions": acts,
                "labels": f.labels.get(&primary),
                "screenshots_taken": shots,
                "guides": guides,
                "image_ref_format": format!("guide/img/{primary}/{}/{}-<n>.webp", source::slug(&f.page.module), source::slug(&f.page.key)),
                "how": format!("Take only the screenshots still missing (the page's opening shot is shoot_pages' job; if it is missing because the path needs an :id, shoot it with params). For each: shoot a spec with the steps that open the form or dialog, then in EVERY guide listed replace the matching <!-- shot: … --> line (places_left) with ![alt](the returned image path) — the {primary} image in every language unless the project shoots per language — and send each guide back with write_doc using its write_doc_item. Change nothing else in the text. The item is done when every language has its screenshots."),
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
                .ok_or_else(|| no_page(cfg, &format!("guide:{lang}:{key}"), key))?;
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
    // the page's screenshots are complete in every language: its shots item is done too
    let shots_done = match (&facts, &page) {
        (Some(f), Some(pg)) => work::shots_short(&cfg, &sources, &pg.key, f)
            .is_empty()
            .then(|| format!("shots:{}", pg.key)),
        _ => None,
    };
    let st = State::update(&cfg, |st| {
        if let Some((key, fp)) = &fp {
            let e = st.pages.entry(key.clone()).or_default();
            e.fingerprint = fp.clone();
            e.documented_fingerprint = Some(fp.clone());
        }
        st.done(item);
        if let Some(s) = &shots_done {
            st.done(s);
        }
        // the suggestion to write process guides is answered by writing one
        if kind == "process" {
            st.done(&format!("process:{lang}:suggest"));
        }
        st.clone()
    })?;
    // only the pages this save touches (a moved guide leaves an old page behind: then all of it)
    let only = [rel.clone()];
    let r = crate::html::build_some(
        &cfg,
        &source::sources(&cfg)?,
        &st,
        if moved.is_empty() { Some(&only[..]) } else { None },
    )?;
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

/// A queued item whose page doctest no longer lists (a route container, a route shadowed by an earlier one with the
/// same path, an excluded page): it leaves the queue, and the answer says so.
fn no_page(cfg: &Config, item: &str, key: &str) -> String {
    let _ = State::update(cfg, |st| {
        st.queue.retain(|q| q != item);
        st.claims.remove(item);
        st.reasons.remove(item);
    });
    format!(
        "no page {key}: doctest does not document it (a route that only holds child pages, a route whose path an earlier route already shows, or an excluded page) — {item} was removed from the queue; claim the next item"
    )
}

/// The marker the guide skeleton leaves where the page's opening screenshot goes.
const PAGE_SHOT_MARKER: &str = "<!-- shot: the page as it opens -->";

/// Puts an image line in a guide's text: in place of the page-shot marker, else after the first paragraph of the
/// body (below the title). Returns None when the text already shows that image.
pub fn insert_page_shot(text: &str, image_line: &str, image_ref: &str) -> Option<String> {
    if text.contains(image_ref) {
        return None;
    }
    if let Some(i) = text.find(PAGE_SHOT_MARKER) {
        return Some(format!(
            "{}{image_line}{}",
            &text[..i],
            &text[i + PAGE_SHOT_MARKER.len()..]
        ));
    }
    // after the front matter
    let body_start = text
        .strip_prefix("---")
        .and_then(|rest| rest.find("\n---").map(|e| 3 + e + 4))
        .unwrap_or(0);
    let lines: Vec<&str> = text[body_start..].split_inclusive('\n').collect();
    let mut pos = body_start;
    let mut seen_text = false;
    for l in &lines {
        let t = l.trim();
        let heading = t.starts_with('#');
        if seen_text && (t.is_empty() || heading) {
            break;
        }
        if !t.is_empty() && !heading {
            seen_text = true;
        }
        pos += l.len();
    }
    let (head, tail) = text.split_at(pos);
    let head = if head.ends_with('\n') {
        head.to_string()
    } else {
        format!("{head}\n")
    };
    Some(format!(
        "{head}\n{image_line}\n{}",
        if tail.starts_with('\n') {
            tail.to_string()
        } else {
            format!("\n{tail}")
        }
    ))
}

/// `shoot_pages`: the opening screenshot of every written page guide, taken by doctest in parallel and put in each
/// language's guide.
fn shoot_pages(p: &mut Project, args: &Value, progress: &dyn crate::progress::Progress) -> Result<String, String> {
    let started = std::time::Instant::now();
    let eff = effective_cfg(p)?;
    let cfg = p.cfg.clone();
    let sources = source::sources(&cfg)?;
    let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let mut fb = FactBuilder::new(&mut *p.api, &cfg, &labels);
    let pages = fb.pages()?;
    drop(fb);
    let primary = cfg.primary_language().to_string();
    let portals: Vec<String> = args
        .get("portals")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(usize::MAX);
    let retake = args.get("retake").and_then(Value::as_bool).unwrap_or(false);
    let mut todo: Vec<(crate::pages::Page, ShotSpec, bool)> = Vec::new();
    let mut need_params = Vec::new();
    for page in &pages {
        if !portals.is_empty() && !portals.contains(&page.portal) {
            continue;
        }
        let Some(g) = work::page_guide(&sources, &page.key, &primary) else {
            continue;
        };
        let text = g.render_text();
        if !text.contains(PAGE_SHOT_MARKER) && work::guide_images(&g.body) > 0 {
            continue;
        }
        if page.path.contains(':') || page.path.contains('{') {
            need_params.push(page.key.clone());
            continue;
        }
        if todo.len() >= limit {
            break;
        }
        let mut spec = ShotSpec {
            id: format!("{}/{}-1", source::slug(&page.module), source::slug(&page.key)),
            page: page.key.clone(),
            path: page.path.clone(),
            lang: primary.clone(),
            ..ShotSpec::default()
        };
        spec.user = shot_user(&eff, &spec);
        let have = crate::shots::image_path(&cfg, &spec).is_file() && !retake;
        todo.push((page.clone(), spec, have));
    }
    let total = todo.len();
    let to_shoot: Vec<(String, ShotSpec)> = todo
        .iter()
        .filter(|(_, _, have)| !have)
        .map(|(pg, s, _)| (pg.key.clone(), s.clone()))
        .collect();
    let already = total - to_shoot.len();
    let workers = cfg.workers.clamp(1, 8);
    let results = shoot_many(&eff, to_shoot, workers, already, total, progress);

    let mut failed: BTreeMap<String, String> = results
        .iter()
        .filter_map(|(k, r)| r.as_ref().err().map(|e| (k.clone(), e.clone())))
        .collect();
    // the same picture for several different pages is a sign-in, error or empty page, not those pages
    let mut by_image: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (k, r) in &results {
        if let Ok(shot) = r
            && let Ok(bytes) = std::fs::read(&shot.path)
        {
            by_image
                .entry(crate::hash::hex(&sha2::Sha256::digest(&bytes)))
                .or_default()
                .push((k.clone(), shot.path.clone()));
        }
    }
    for same in by_image.values().filter(|v| v.len() >= 3) {
        for (k, path) in same {
            let _ = std::fs::remove_file(path);
            failed.insert(
                k.clone(),
                format!(
                    "the screenshot is identical to {} other pages' (likely a sign-in, error or empty page): not used — check the page or its portal's sign-in (login), then shoot it",
                    same.len() - 1
                ),
            );
        }
    }
    // put each image in every language's guide
    let root = source::source_root(&cfg);
    let mut placed = 0;
    let mut guides = 0;
    let mut taken: Vec<ShotSpec> = Vec::new();
    for (page, spec, _) in &todo {
        if failed.contains_key(&page.key) {
            continue;
        }
        taken.push(spec.clone());
        let image_ref = crate::shots::image_ref(spec);
        let mut any = false;
        for lang in &cfg.languages {
            let Some(g) = work::page_guide(&sources, &page.key, lang) else {
                continue;
            };
            let file = root.join(&g.path);
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let line = format!("![{}]({image_ref})", g.fm.title.replace(['[', ']'], ""));
            if let Some(new) = insert_page_shot(&text, &line, &image_ref) {
                std::fs::write(&file, new).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
                guides += 1;
                any = true;
            }
        }
        if any {
            placed += 1;
        }
    }
    let sources = source::sources(&cfg)?;
    let st = State::update(&cfg, |st| {
        for spec in &taken {
            st.shots.insert(spec.id.clone(), spec.clone());
            let e = st.pages.entry(spec.page.clone()).or_default();
            if !e.shots.contains(&spec.id) {
                e.shots.push(spec.id.clone());
            }
            st.done(&format!("shot:{}", spec.id));
        }
        st.clone()
    })?;
    let built = crate::html::build(&cfg, &sources, &st)?;
    let shot_now = results.len() - failed.len();
    let mut out = format!(
        "{} page(s): {shot_now} screenshot(s) taken with {workers} browser(s), {} already there; put in {guides} guide(s) of {placed} page(s); built {} files in {}",
        total,
        total - results.len(),
        built.written.len(),
        took(started.elapsed())
    );
    if !failed.is_empty() {
        out += &format!("\nfailed ({}):", failed.len());
        for (k, e) in failed.iter().take(40) {
            out += &format!("\n- {k}: {e}");
        }
        if failed.len() > 40 {
            out += &format!("\n- … and {} more", failed.len() - 40);
        }
    }
    if !need_params.is_empty() {
        out += &format!(
            "\nneed an :id, left for their shots:<page> item ({}): {}",
            need_params.len(),
            need_params.iter().take(20).cloned().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(out)
}

/// Takes these specs with `workers` browsers at once; (key, result) for each. Progress counts from `already` of
/// `total`.
fn shoot_many(
    eff: &Config,
    specs: Vec<(String, ShotSpec)>,
    workers: usize,
    already: usize,
    total: usize,
    progress: &dyn crate::progress::Progress,
) -> Vec<(String, Result<crate::shots::Shot, String>)> {
    let queue = std::sync::Mutex::new(specs);
    let results: std::sync::Mutex<Vec<(String, Result<crate::shots::Shot, String>)>> =
        std::sync::Mutex::new(Vec::new());
    let done = std::sync::atomic::AtomicUsize::new(already);
    std::thread::scope(|sc| {
        for _ in 0..workers.max(1) {
            sc.spawn(|| {
                let mut shooter = match Shooter::start(eff) {
                    Ok(s) => s,
                    Err(e) => {
                        let mut q = queue.lock().expect("queue");
                        let mut r = results.lock().expect("results");
                        for (key, _) in q.drain(..) {
                            r.push((key, Err(e.clone())));
                        }
                        return;
                    }
                };
                loop {
                    let Some((key, spec)) = queue.lock().expect("queue").pop() else {
                        break;
                    };
                    let r = shooter.capture(&spec);
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    progress.step(
                        n,
                        total,
                        &format!("{} ({})", key, if r.is_ok() { "ok" } else { "failed" }),
                    );
                    results.lock().expect("results").push((key, r));
                }
            });
        }
    });
    results.into_inner().expect("results")
}

/// `retake`: stored screenshots taken again, several browsers at once (after a [screenshots] change: a new mask,
/// colour scheme or width): all, those of pages matching a pattern, or the ids given.
fn retake(p: &mut Project, args: &Value, progress: &dyn crate::progress::Progress) -> Result<String, String> {
    let started = std::time::Instant::now();
    let eff = effective_cfg(p)?;
    let st = State::load(&p.cfg);
    let strings = |k: &str| -> Vec<String> {
        args.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let ids = strings("ids");
    let pages = strings("pages");
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    if ids.is_empty() && pages.is_empty() && !all {
        return Err("say which: ids: [...], pages: [route patterns], or all: true".into());
    }
    let specs: Vec<(String, ShotSpec)> = st
        .shots
        .values()
        .filter(|s| all || ids.contains(&s.id) || crate::work::page_matches(&s.page, &s.path, &pages))
        .map(|s| {
            let mut s = s.clone();
            s.user = shot_user(&eff, &s);
            (s.id.clone(), s)
        })
        .collect();
    let total = specs.len();
    let workers = args
        .get("workers")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(p.cfg.workers)
        .clamp(1, 8);
    let results = shoot_many(&eff, specs, workers, 0, total, progress);
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(k, r)| r.as_ref().err().map(|e| format!("{k}: {e}")))
        .collect();
    let mut out = format!(
        "retook {} of {total} screenshot(s) with {workers} browser(s) in {}",
        total - failed.len(),
        took(started.elapsed())
    );
    if !failed.is_empty() {
        out += &format!("\nfailed ({}):", failed.len());
        for f in failed.iter().take(40) {
            out += &format!("\n- {f}");
        }
    }
    Ok(out)
}

/// `shoot {retake_page, lang}`: every stored screenshot of a page, retaken in another language.
fn retake_page(p: &mut Project, page: &str, lang: &str) -> Result<Vec<Value>, String> {
    check_lang(&p.cfg, lang)?;
    let st = State::load(&p.cfg);
    let primary = p.cfg.primary_language().to_string();
    let specs: Vec<ShotSpec> = st
        .shots
        .values()
        .filter(|s| s.page == page && s.lang == primary)
        .cloned()
        .collect();
    if specs.is_empty() {
        return Err(format!(
            "no stored screenshots of {page} in {primary}: take them for the {primary} guide first"
        ));
    }
    let eff = effective_cfg(p)?;
    if p.shooter.is_none() {
        p.shooter = Some(Shooter::start(&eff)?);
    }
    let mut lines = Vec::new();
    let mut blocked = 0;
    for mut spec in specs {
        spec.lang = lang.to_string();
        spec.user = shot_user(&eff, &spec);
        let shot = p.shooter.as_mut().expect("started").capture(&spec)?;
        blocked += shot.blocked.len();
        lines.push(format!("- ![…]({})", crate::shots::image_ref(&spec)));
    }
    Ok(vec![json!({"type": "text", "text": format!(
        "retook {} screenshot(s) of {page} in {lang}{}:\n{}",
        lines.len(),
        if blocked > 0 { format!(" (safe mode blocked {blocked} save request(s))") } else { String::new() },
        lines.join("\n")
    )})])
}

fn shoot(p: &mut Project, args: &Value) -> Result<Vec<Value>, String> {
    if let Some(page) = s(args, "retake_page") {
        let lang = s(args, "lang").ok_or("retake_page needs lang")?.to_string();
        return retake_page(p, page, &lang);
    }
    let st = State::load(&p.cfg);
    let mut notes: Vec<String> = Vec::new();
    // a spec object, the spec's fields sent flat, or an id alone (retake the stored spec)
    let flat = ["path", "steps", "crop", "mask", "params", "lang", "page"]
        .iter()
        .any(|k| args.get(*k).is_some());
    let spec: ShotSpec = match (args.get("spec"), s(args, "id")) {
        (Some(v), _) if v.is_object() => serde_json::from_value(v.clone()).map_err(|e| format!("bad spec: {e}"))?,
        _ if flat => {
            let mut v = args.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("project");
            }
            serde_json::from_value(v).map_err(|e| format!("bad spec: {e}"))?
        }
        (_, Some(id)) => {
            let stored = st
                .shots
                .get(id)
                .cloned()
                .ok_or_else(|| format!("no stored spec {id}; send a spec"))?;
            notes.push(format!(
                "retook the stored spec of {id} (path {}, {} step(s)); to change it, send the whole spec",
                stored.path,
                stored.steps.len()
            ));
            stored
        }
        _ => return Err("send spec or id".into()),
    };
    let mut spec = spec;
    let eff = effective_cfg(p)?;
    spec.user = shot_user(&eff, &spec);
    crate::shots::validate_spec(&spec)?;
    crate::shots::resolve_path(&spec)?;
    check_lang(&p.cfg, &spec.lang)?;
    if spec.allow_saves && eff.screenshots.safe && st.shots.contains_key(&spec.id) {
        notes.push(format!(
            "warning: {} lets its steps save data, and it was taken before: this retake saved again (check for a duplicate record)",
            spec.id
        ));
    }
    if p.shooter.is_none() {
        p.shooter = Some(Shooter::start(&eff)?);
    }
    let shot = p.shooter.as_mut().expect("started").capture(&spec)?;
    State::update(&p.cfg, |st| {
        st.shots.insert(spec.id.clone(), spec.clone());
        let e = st.pages.entry(spec.page.clone()).or_default();
        if !e.shots.contains(&spec.id) {
            e.shots.push(spec.id.clone());
        }
        st.done(&format!("shot:{}", spec.id));
    })?;
    if !shot.blocked.is_empty() {
        notes.push(format!(
            "safe mode blocked {}: {} — nothing was saved. If the screen shows an error because of it, take the shot before that step; to show a real save, send allow_saves: true (test data only); for a request that only reads (a search sent as POST), add it to [screenshots] allow",
            if shot.blocked.len() == 1 { "a request".to_string() } else { format!("{} requests", shot.blocked.len()) },
            shot.blocked.join(", ")
        ));
    }
    if !shot.unmatched_masks.is_empty() {
        notes.push(format!(
            "masks that matched nothing on this page: {} (fine if the page has no such data; else fix the selector or regex)",
            shot.unmatched_masks.join(", ")
        ));
    }
    if !shot.dialogs.is_empty() {
        notes.push(format!(
            "the page opened {} and doctest answered: {} — if one is an error, the screenshot may show the wrong state",
            if shot.dialogs.len() == 1 {
                "a dialog".to_string()
            } else {
                format!("{} dialogs", shot.dialogs.len())
            },
            shot.dialogs.join("; ")
        ));
    }
    let bytes = std::fs::read(&shot.path).map_err(|e| e.to_string())?;
    let (w, h) = image::image_dimensions(&shot.path).unwrap_or((0, 0));
    let show = args.get("show").and_then(Value::as_bool).unwrap_or(false);
    let mut text = format!(
        "saved {} ({w}x{h}, {} KB); reference it as ![…]({}){}",
        shot.path.display(),
        bytes.len() / 1024,
        crate::shots::image_ref(&spec),
        if show {
            ""
        } else {
            " — the image is not sent back (send show: true to see it)"
        }
    );
    for n in &notes {
        text += &format!("\n- {n}");
    }
    let mut out = vec![json!({"type": "text", "text": text})];
    if show {
        out.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": "image/webp"}));
    }
    Ok(out)
}

/// facts without what a worker can read itself or rarely needs: the current guide text (its length and path stay),
/// component lists and long notes.
fn brief_facts(full: &str) -> String {
    let end = full.rfind('}').map(|i| i + 1).unwrap_or(full.len());
    let Ok(mut v) = serde_json::from_str::<Value>(&full[..end]) else {
        return full.to_string();
    };
    if let Some(o) = v.as_object_mut() {
        if let Some(cur) = o.remove("current_text") {
            let n = cur.as_str().map(str::len).unwrap_or(0);
            if n > 0 {
                o.insert(
                    "current_text".into(),
                    json!(format!(
                        "({n} characters; read the guide at source_path, or call facts without brief)"
                    )),
                );
            }
        }
        o.retain(|k, _| !k.ends_with("_note") && k != "components" && k != "screenshot_plan_without_data");
    }
    format!("{}{}", pretty(&v), &full[end..])
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
        Ok((e.action.clone(), cases::for_endpoint(&e, Some(&cfg.backend)), info))
    } else {
        let (sym, src) = fb.symbol_source(target)?.ok_or_else(|| format!("no method {target}"))?;
        let info = json!({"method": target, "file": sym["file"], "line": sym["line"], "signature": sym["signature"], "source": src});
        Ok((target.to_string(), cases::for_method(target, &src), info))
    }
}

/// The tests covering `target`, read the way status reads them: doctest's and laravel-parity's rows from the files,
/// hand-written tests as the last plan matched them against every route (a request to `orders/create` belongs to that
/// route, not to `orders/{order}`).
fn coverage_ledger(cfg: &Config, target: &str) -> Vec<crate::state::Covered> {
    let mut ledger = crate::ledger::scan(cfg, &[]);
    let inferred: Vec<crate::state::Covered> = State::load(cfg)
        .ledger
        .into_iter()
        .filter(|c| c.source == CoverSource::Inferred)
        .collect();
    if inferred.is_empty() {
        return crate::ledger::scan(cfg, &endpoint_for_ledger(target));
    }
    ledger.extend(inferred);
    ledger
}

fn cases_text(p: &mut Project, target: &str) -> Result<String, String> {
    let (action, expected, info) = target_info(p, target)?;
    let cfg = p.cfg.clone();
    let ledger = coverage_ledger(&cfg, target);
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
        "how": "write_test with one parameterised method (e.g. test_<action>) and one row per missing case; args_php are the row's values in the method's parameter order. doctest writes the data provider cases_<method> from the rows itself: method_php is the test method alone, with no DataProvider attribute or provider method. Use factories and actingAs for the user; positive and negative cases in the same provider. Code that cannot run under PHPUnit (exit(), dd(), real network calls, a package's route): write_test with skip: \"why\".",
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

/// One write_test item, checked and ready to write: its rows, its test file and class.
struct PreparedTest {
    target: String,
    t: NewTests,
    replace: bool,
    replace_php: Option<String>,
    removed_note: String,
    file: PathBuf,
    class: String,
    ns: String,
    rel: String,
}

enum Prepared {
    /// Nothing to run (skip, unskip, a removal only): the reply.
    Done(String),
    Ready(Box<PreparedTest>),
}

fn write_test(p: &mut Project, args: &Value) -> Result<String, String> {
    let cfg = p.cfg.clone();
    // several targets at once: one test run for all of them
    if let Some(list) = args.get("tests").and_then(Value::as_array) {
        if list.is_empty() {
            return Err("tests is empty".into());
        }
        let mut replies: Vec<(usize, String, Result<String, String>)> = Vec::new();
        let mut ready: Vec<(usize, PreparedTest)> = Vec::new();
        for (i, item) in list.iter().enumerate() {
            let target = s(item, "target").unwrap_or("?").to_string();
            match prepare_test(p, item) {
                Ok(Prepared::Done(m)) => replies.push((i, target, Ok(m))),
                Ok(Prepared::Ready(r)) => ready.push((i, *r)),
                Err(e) => replies.push((i, target, Err(e))),
            }
        }
        replies.extend(write_prepared(&cfg, ready));
        replies.sort_by_key(|(i, _, _)| *i);
        let failed = replies.iter().filter(|r| r.2.is_err()).count();
        let mut out = format!(
            "{} target(s): {} written, {failed} not written",
            replies.len(),
            replies.len() - failed
        );
        for (_, target, r) in &replies {
            match r {
                Ok(m) => out += &format!("\n\n## {target}\n{m}"),
                Err(e) => out += &format!("\n\n## {target}\nnot written: {e}"),
            }
        }
        return if failed == replies.len() { Err(out) } else { Ok(out) };
    }
    match prepare_test(p, args)? {
        Prepared::Done(m) => Ok(m),
        Prepared::Ready(r) => write_prepared(&cfg, vec![(0, *r)])
            .pop()
            .map(|(_, _, r)| r)
            .unwrap_or_else(|| Err("nothing was written".into())),
    }
}

/// Checks one write_test item and does what needs no test run (skip, unskip, removing rows).
fn prepare_test(p: &mut Project, args: &Value) -> Result<Prepared, String> {
    let target = s(args, "target").ok_or("target is required")?.to_string();
    let cfg = p.cfg.clone();
    // a target PHPUnit cannot test: it leaves the queue for good
    if let Some(why) = s(args, "skip") {
        let why = why.trim().to_string();
        if why.is_empty() {
            return Err("skip needs the reason (e.g. \"calls exit()\")".into());
        }
        State::update(&cfg, |st| {
            st.untestable.insert(target.clone(), why.clone());
            st.done(&format!("tests:{target}"));
        })?;
        return Ok(Prepared::Done(format!(
            "{target} is marked as not testable ({why}); status and claim no longer list it (unskip: true undoes it)"
        )));
    }
    if args.get("unskip").and_then(Value::as_bool).unwrap_or(false) {
        let was = State::update(&cfg, |st| st.untestable.remove(&target))?;
        return Ok(Prepared::Done(match was {
            Some(_) => format!("{target} is testable again; the next status lists its missing cases"),
            None => format!("{target} was not marked as not testable"),
        }));
    }
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
    let mut replace = args.get("replace").and_then(Value::as_bool).unwrap_or(false);
    let mut removed_note = String::new();
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
        {
            let _file_lock = testwrite::file_lock(&file)?;
            testwrite::remove_rows_unlocked(&file, &t.method, &remove)?;
            let still = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| testwrite::function_span(&text, &t.method))
                .is_some();
            if !still && replace {
                // every row went, and the method with them: the new body is written as a new method
                replace = false;
                removed_note = ", every old row removed so the method was written anew".into();
            }
        }
        if t.rows.is_empty() && !replace {
            if t.method_php.is_some() && !removed_note.is_empty() {
                return Err(format!(
                    "removed rows {} and with them {}; send method_php again with rows to write it anew",
                    remove.join(", "),
                    t.method
                ));
            }
            State::update(&cfg, |st| st.done(&format!("tests:{target}")))?;
            return Ok(Prepared::Done(format!(
                "removed rows {} from {}",
                remove.join(", "),
                t.method
            )));
        }
    }
    let (action, _, _) = target_info(p, &target)?;
    let ledger = coverage_ledger(&cfg, &target);
    let (file, class, ns) = testwrite::target_file(&cfg, &target, &action);
    let own = format!("{class}::{}", t.method);
    // a case covered by this same doctest method is a row moving between its providers (bug note added or taken
    // away), or a duplicate the insert refuses with its own message
    let covered: Vec<String> = t
        .rows
        .iter()
        .filter_map(|r| {
            cases::covered_by(&ledger, &target, &r.case)
                .filter(|c| !(c.source == CoverSource::Doctest && c.test == own))
                .map(|c| format!("{} (by {})", r.case, c.test))
        })
        .collect();
    if !covered.is_empty() {
        return Err(format!("already covered, not written: {}", covered.join(", ")));
    }
    let mut t = t;
    let replace_php = if replace {
        Some(t.method_php.take().ok_or("replace needs method_php")?)
    } else {
        None
    };
    let rel = file
        .strip_prefix(&cfg.backend)
        .unwrap_or(&file)
        .to_string_lossy()
        .into_owned();
    Ok(Prepared::Ready(Box::new(PreparedTest {
        target,
        t,
        replace,
        replace_php,
        removed_note,
        file,
        class,
        ns,
        rel,
    })))
}

/// Writes prepared items, runs them (one PHPUnit run per group of items whose method names differ, only their test
/// files), keeps the rows whose own data set passed and takes the others out again. (index, target, reply).
fn write_prepared(cfg: &Config, items: Vec<(usize, PreparedTest)>) -> Vec<(usize, String, Result<String, String>)> {
    // PHPUnit's output names a test by its method only: items sharing a method name go in separate runs
    let mut runs: Vec<Vec<(usize, PreparedTest)>> = Vec::new();
    for item in items {
        let m = item.1.t.method.clone();
        match runs.iter_mut().find(|r| r.iter().all(|(_, x)| x.t.method != m)) {
            Some(r) => r.push(item),
            None => runs.push(vec![item]),
        }
    }
    let mut out = Vec::new();
    for run in runs {
        out.extend(write_run(cfg, run));
    }
    out
}

fn write_run(cfg: &Config, run: Vec<(usize, PreparedTest)>) -> Vec<(usize, String, Result<String, String>)> {
    let mut out: Vec<(usize, String, Result<String, String>)> = Vec::new();
    // the whole edit → run → keep/take out sequence holds the files and a test runner slot: no other worker's run
    // loads these files half-edited, and taking rows out never drops another worker's edit
    let mut files: Vec<PathBuf> = run.iter().map(|(_, x)| x.file.clone()).collect();
    files.sort();
    files.dedup();
    let mut locks = Vec::new();
    for f in &files {
        match testwrite::file_lock(f) {
            Ok(l) => locks.push(l),
            Err(e) => {
                return run.into_iter().map(|(i, x)| (i, x.target, Err(e.clone()))).collect();
            }
        }
    }
    let (_slot_lock, slot) = match State::test_slot(cfg) {
        Ok(s) => s,
        Err(e) => return run.into_iter().map(|(i, x)| (i, x.target, Err(e.clone()))).collect(),
    };
    let snapshot: HashMap<PathBuf, Option<String>> = files
        .iter()
        .map(|f| (f.clone(), std::fs::read_to_string(f).ok()))
        .collect();
    let put_back = |file: &PathBuf, text: &Option<String>| {
        let _ = match text {
            Some(t) => testwrite::write_atomic(file, t),
            None => std::fs::remove_file(file).map_err(|e| e.to_string()),
        };
    };
    // edit every item; one that cannot be written leaves its file as it was before it
    struct Written {
        idx: usize,
        x: PreparedTest,
        before: Option<String>,
        main_rows: Vec<String>,
        bug_rows: Vec<String>,
    }
    let mut written: Vec<Written> = Vec::new();
    for (idx, mut x) in run {
        let before = std::fs::read_to_string(&x.file).ok();
        let edit = || -> Result<(), String> {
            if let Some(php) = x.replace_php.as_deref() {
                testwrite::replace_method_unlocked(&x.file, &x.t.method, php)?;
            }
            if !x.t.rows.is_empty() {
                testwrite::insert_unlocked(&x.file, &x.class, &x.ns, &x.target, &x.t)?;
            }
            Ok(())
        };
        if let Err(e) = edit() {
            let changed = before != std::fs::read_to_string(&x.file).ok();
            if changed {
                put_back(&x.file, &before);
            }
            out.push((
                idx,
                x.target.clone(),
                Err(if changed || x.replace {
                    format!("the test file was rolled back to how it was: {e}")
                } else {
                    e
                }),
            ));
            continue;
        }
        let text = std::fs::read_to_string(&x.file).unwrap_or_default();
        let rows = |p: String| -> Vec<String> {
            testwrite::provider_rows(&text, &p)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        let main_rows = rows(format!("cases_{}", x.t.method));
        let bug_rows = rows(format!("bugs_{}", x.t.method));
        if main_rows.is_empty() && bug_rows.is_empty() {
            put_back(&x.file, &before);
            out.push((idx, x.target.clone(), Err(format!("{} has no rows to run", x.t.method))));
            continue;
        }
        x.replace_php = None;
        written.push(Written {
            idx,
            x,
            before,
            main_rows,
            bug_rows,
        });
    }
    if written.is_empty() {
        return out;
    }
    let mut by_class: Vec<(String, Vec<String>)> = Vec::new();
    for w in &written {
        let mut ms = Vec::new();
        if !w.main_rows.is_empty() {
            ms.push(w.x.t.method.clone());
        }
        if !w.bug_rows.is_empty() {
            ms.push(format!("{}_known_bugs", w.x.t.method));
        }
        match by_class.iter_mut().find(|(c, _)| *c == w.x.class) {
            Some((_, v)) => v.extend(ms),
            None => by_class.push((w.x.class.clone(), ms)),
        }
    }
    let mut rels: Vec<String> = written.iter().map(|w| w.x.rel.clone()).collect();
    rels.sort();
    rels.dedup();
    let res = match testwrite::run_in(cfg, &rels, &testwrite::filter_for_many(&by_class), slot) {
        Ok(r) => r,
        Err(e) => {
            for (f, text) in &snapshot {
                put_back(f, text);
            }
            for w in written {
                out.push((
                    w.idx,
                    w.x.target,
                    Err(format!("the test file was rolled back to how it was: {e}")),
                ));
            }
            return out;
        }
    };
    let shared: HashMap<PathBuf, usize> = written.iter().fold(HashMap::new(), |mut m, w| {
        *m.entry(w.x.file.clone()).or_default() += 1;
        m
    });
    let mut done_targets: Vec<(String, String)> = Vec::new();
    for w in written {
        let Written {
            idx,
            x,
            before,
            main_rows,
            bug_rows,
        } = w;
        let t = &x.t;
        let bugs_method = format!("{}_known_bugs", t.method);
        // takes this item out again: its file as it was when it is the only item there, else its own rows (and,
        // for a replaced body, the old body)
        let undo = || {
            if shared.get(&x.file).copied().unwrap_or(0) <= 1 {
                put_back(&x.file, &before);
                return;
            }
            let cases: Vec<String> = t.rows.iter().map(|r| r.case.clone()).collect();
            let _ = testwrite::remove_rows_unlocked(&x.file, &t.method, &cases);
            if x.replace
                && let Some(old) = before.as_deref()
                && let Some((s0, e0)) = testwrite::function_span(old, &t.method)
                && let Ok(now) = std::fs::read_to_string(&x.file)
                && let Some((s1, e1)) = testwrite::function_span(&now, &t.method)
            {
                let mut fixed = now.clone();
                fixed.replace_range(s1..e1, &old[s0..e0]);
                let _ = testwrite::write_atomic(&x.file, &fixed);
            }
        };
        // every row is judged by its own data set: passed, failed, or not run at all
        let status = |case: &str| -> Option<bool> {
            if bug_rows.iter().any(|b| b == case) {
                res.row(&bugs_method, case)
            } else {
                res.row(&t.method, case)
            }
        };
        let ran_any = main_rows.iter().chain(&bug_rows).any(|c| status(c).is_some());
        if !ran_any {
            undo();
            out.push((
                idx,
                x.target.clone(),
                Err(format!(
                    "the test file was rolled back to how it was: no row of {} ran (a data provider that cannot be built, e.g. row values that need the framework such as __() or factories, or a file that does not parse):\n{}",
                    t.method,
                    testwrite::tail(&res.output, 30)
                )),
            ));
            continue;
        }
        let detail = |case: &str, method: &str| -> String {
            match status(case) {
                None => format!(
                    "did not run (its data set was not in PHPUnit's output):\n{}",
                    testwrite::tail(&res.output, 12)
                ),
                _ => {
                    let (at, text) = res.failure(method, case);
                    let text = if text.trim().is_empty() {
                        testwrite::tail(&res.output, 15)
                    } else {
                        text.lines().take(12).collect::<Vec<_>>().join("\n")
                    };
                    format!("{}: {}", at.as_deref().unwrap_or("no file:line printed"), text)
                }
            }
        };
        let new_cases: Vec<&str> = t
            .rows
            .iter()
            .filter(|r| r.bug.is_none())
            .map(|r| r.case.as_str())
            .collect();
        let old_failing: Vec<&String> = main_rows
            .iter()
            .filter(|c| !new_cases.contains(&c.as_str()) && status(c) != Some(true))
            .collect();
        if x.replace && !old_failing.is_empty() {
            let lines: Vec<String> = old_failing
                .iter()
                .map(|c| format!("- {c}: {}", detail(c, &t.method)))
                .collect();
            undo();
            out.push((
                idx,
                x.target.clone(),
                Err(format!(
                    "the test file was rolled back to how it was: the new method_php fails rows {} — fix it and send it again:\n{}",
                    old_failing.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
                    lines.join("\n")
                )),
            ));
            continue;
        }
        let to_remove: Vec<String> = new_cases
            .iter()
            .filter(|c| status(c) != Some(true))
            .map(|c| c.to_string())
            .collect();
        let mut reply = format!(
            "{}: {} row(s) written{}{}",
            x.rel,
            t.rows.len() - to_remove.len(),
            if x.replace { ", method replaced" } else { "" },
            x.removed_note
        );
        for c in &to_remove {
            reply += &format!("\n- {c} failed and was removed ({})", detail(c, &t.method));
        }
        if !to_remove.is_empty()
            && let Err(e) = testwrite::remove_rows_unlocked(&x.file, &t.method, &to_remove)
        {
            undo();
            out.push((
                idx,
                x.target.clone(),
                Err(format!("the test file was rolled back to how it was: {e}")),
            ));
            continue;
        }
        let passing_bugs: Vec<&String> = bug_rows.iter().filter(|c| status(c) == Some(true)).collect();
        if !old_failing.is_empty() {
            reply += &format!(
                "\n- rows already in {} that fail now (not removed; fix with replace or remove): {}",
                t.method,
                old_failing.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        if !passing_bugs.is_empty() {
            reply += &format!(
                "\n- marked as bugs but passing (not bugs; send them again without bug to move them): {}",
                passing_bugs.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        if !to_remove.is_empty() {
            reply += "\nRead the code for each removed case: if the code is wrong, send the row again with bug set; if the test was wrong, fix args_php.";
        }
        done_targets.push((x.target.clone(), t.method.clone()));
        out.push((idx, x.target.clone(), Ok(reply)));
    }
    drop(locks);
    if !done_targets.is_empty() {
        let _ = State::update(cfg, |st| {
            for (target, method) in &done_targets {
                st.failing.retain(|f| !testwrite::same_method(f, method));
                st.done(&format!("tests:{target}"));
            }
        });
    }
    out
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
    // empty data providers older versions left would fail every run, filtered or not
    let repaired = testwrite::repair_empty(&cfg);
    let res = {
        let (_lock, slot) = State::test_slot(&cfg)?;
        testwrite::run(&cfg, &filter, slot)?
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
        "{} passed, {} failed{}{}",
        res.passed.len(),
        res.failed.len(),
        if points.is_empty() {
            String::new()
        } else {
            format!("\n- {}", points.join("\n- "))
        },
        if repaired.is_empty() {
            String::new()
        } else {
            format!(
                "\nrepaired {} doctest method(s) with an empty data provider (left by an older doctest; PHPUnit failed them on every run): {}",
                repaired.len(),
                repaired.join(", ")
            )
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
    // Chromes left behind by a doctest that was killed: stop them now and every minute
    std::thread::spawn(|| {
        loop {
            let n = crate::shots::sweep_orphans();
            if n > 0 {
                eprintln!("doctest: stopped {n} Chrome(s) left behind by a doctest that is gone");
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
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
                crate::codemap::save_memo();
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
    // the client went away: stop our Chromes and leave, without waiting for calls nobody will read
    drop(tx);
    drop(handles);
    crate::shots::kill_own();
    std::process::exit(0);
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
    #[ignore = "needs Chrome"]
    fn shoot_pages_takes_each_written_page_and_puts_it_in_every_language() {
        let port = crate::shots::tests::serve();
        let (_d, cfg) = project();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + &format!("[app]\nbase_url=\"http://127.0.0.1:{port}\"\n");
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let cfg = Config::find(&cfg.root).unwrap();
        let src = cfg.out.join("_source/guide");
        for (lang, title, body) in [
            ("en", "New Order", "Create an order."),
            ("id", "Pesanan Baru", "Buat pesanan."),
        ] {
            std::fs::create_dir_all(src.join(lang).join("orders")).unwrap();
            std::fs::write(
                src.join(lang).join("orders/shop-orders-create.md"),
                format!("---\ntitle: {title}\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\n# {title}\n\n{body}\n\n<!-- shot: the page as it opens -->\n\n## Add\n1. Click **Save**.\n"),
            )
            .unwrap();
        }
        let mut s = server();
        let (text, err) = call(&mut s, "shoot_pages", json!({"project": cfg.root}));
        assert!(
            !err && text.contains("1 screenshot(s) taken") && text.contains("put in 2 guide(s)"),
            "{text}"
        );
        let image = "guide/img/en/orders/shop-orders-create-1.webp";
        assert!(cfg.out.join(image).is_file());
        let en = std::fs::read_to_string(src.join("en/orders/shop-orders-create.md")).unwrap();
        let id = std::fs::read_to_string(src.join("id/orders/shop-orders-create.md")).unwrap();
        assert!(
            en.contains(&format!("![New Order]({image})")) && !en.contains("<!-- shot: the page"),
            "{en}"
        );
        assert!(id.contains(&format!("![Pesanan Baru]({image})")), "{id}");
        // a second run finds nothing left to take
        let (text, _) = call(&mut s, "shoot_pages", json!({"project": cfg.root}));
        assert!(text.starts_with("0 page(s)"), "{text}");
    }

    #[test]
    fn background_work_comes_from_the_app_folders() {
        let d = tempfile::tempdir().unwrap();
        let b = d.path();
        std::fs::create_dir_all(b.join("app/Jobs/Tax")).unwrap();
        std::fs::create_dir_all(b.join("app/Console/Commands")).unwrap();
        std::fs::create_dir_all(b.join("routes")).unwrap();
        std::fs::write(
            b.join("app/Jobs/Tax/SubmitTax.php"),
            "<?php\nclass SubmitTax implements ShouldQueue {}\n",
        )
        .unwrap();
        std::fs::write(
            b.join("app/Console/Commands/Renew.php"),
            "<?php\nclass Renew extends Command {\n protected $signature = 'tenancy:renew\n   {--dry}';\n protected $description = \"Renews tenancies\";\n}\n",
        )
        .unwrap();
        std::fs::write(
            b.join("routes/console.php"),
            "<?php\nSchedule::command('tenancy:renew')->daily();\n// Schedule::command('old')\n",
        )
        .unwrap();
        let (jobs, commands, schedule) = background_work(b);
        assert_eq!(jobs[0]["class"], "SubmitTax");
        assert_eq!(jobs[0]["queued"], true);
        assert_eq!(commands[0]["signature"], "tenancy:renew {--dry}");
        assert_eq!(commands[0]["description"], "Renews tenancies");
        assert_eq!(
            schedule,
            vec!["routes/console.php:2: Schedule::command('tenancy:renew')->daily();"]
        );
    }

    #[test]
    fn page_shot_goes_at_its_marker_or_after_the_first_paragraph() {
        let img = "![Orders](guide/img/en/o/p-1.webp)";
        let r = "guide/img/en/o/p-1.webp";
        let marked =
            "---\ntitle: Orders\n---\n# Orders\n\nLists orders.\n\n<!-- shot: the page as it opens -->\n\n## Add\n";
        assert_eq!(
            insert_page_shot(marked, img, r).unwrap(),
            "---\ntitle: Orders\n---\n# Orders\n\nLists orders.\n\n![Orders](guide/img/en/o/p-1.webp)\n\n## Add\n"
        );
        let plain = "---\ntitle: Orders\n---\n# Orders\n\nLists orders\nof the shop.\n\n## Add\n1. Click.\n";
        assert_eq!(
            insert_page_shot(plain, img, r).unwrap(),
            "---\ntitle: Orders\n---\n# Orders\n\nLists orders\nof the shop.\n\n![Orders](guide/img/en/o/p-1.webp)\n\n## Add\n1. Click.\n"
        );
        let done = insert_page_shot(plain, img, r).unwrap();
        assert!(insert_page_shot(&done, img, r).is_none(), "already there");
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
                "shoot_pages",
                "retake",
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
    fn write_doc_reads_markdown_from_a_file_and_facts_can_be_brief() {
        let (d, cfg) = project();
        let mut s = server();
        let f = d.path().join("guide.md");
        std::fs::write(
            &f,
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Save**.\n",
        )
        .unwrap();
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown_file": f}),
        );
        assert!(!err, "{text}");
        assert!(cfg.out.join("guide/en/orders/shop-orders-create.html").is_file());
        let (full, _) = call(
            &mut s,
            "facts",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create"}),
        );
        let (brief, err) = call(
            &mut s,
            "facts",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "brief": true}),
        );
        assert!(!err, "{brief}");
        assert!(
            full.contains("Click **Save**") && !brief.contains("Click **Save**"),
            "{brief}"
        );
        assert!(
            brief.contains("characters; read the guide at source_path") && brief.contains("\"labels\""),
            "{brief}"
        );
        let (text, err) = call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create"}),
        );
        assert!(err && text.contains("markdown_file"), "{text}");
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
    fn write_test_judges_each_row_by_its_own_data_set() {
        let (_d, cfg) = project();
        // coloured output; the failing key has an underscore that Collision prints as a space; one row never ran
        std::fs::write(
            cfg.backend.join("fake.txt"),
            "  \x1b[32m✓\x1b[39m store with data set \"forbidden\"\n  \x1b[31m⨯\x1b[39m store with data set \"returns:partly paid\"\n   FAILED  Tests\\Feature\\Http\\Controllers\\OrderControllerTest > store with data set \"returns:partly paid\"\n  Failed asserting that 'PAID' is identical to 'PARTLY'.\n\n  at tests/Feature/Http/Controllers/OrderControllerTest.php:31\n\n  Tests: 1 failed, 1 passed\n",
        )
        .unwrap();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + "[tests]\nrun = \"cat fake.txt # {filter}\"\n";
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let mut s = server();
        let args = json!({
            "project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store",
            "method_php": "#[\\PHPUnit\\Framework\\Attributes\\DataProvider('mine')]\npublic function test_store(int $s): void {}",
            "rows": [{"case": "forbidden", "args_php": "403"}, {"case": "returns:partly_paid", "args_php": "200"}, {"case": "state:paid", "args_php": "401"}]
        });
        let (text, err) = call(&mut s, "write_test", args);
        assert!(!err, "{text}");
        assert!(text.contains("returns:partly_paid failed and was removed"), "{text}");
        assert!(
            text.contains("OrderControllerTest.php:31") && text.contains("PARTLY"),
            "{text}"
        );
        assert!(
            text.contains("state:paid failed and was removed (did not run"),
            "{text}"
        );
        let php = std::fs::read_to_string(
            cfg.backend
                .join("tests/Feature/Http/Controllers/OrderControllerTest.php"),
        )
        .unwrap();
        assert!(
            php.contains("'forbidden' => [403]") && !php.contains("partly_paid") && !php.contains("state:paid"),
            "{php}"
        );
        assert!(!php.contains("'mine'"), "{php}");
    }

    #[test]
    fn write_test_batch_runs_several_targets_once() {
        let (_d, cfg) = project();
        // one run's output for both methods; `{slot}` picks the output file of the slot
        std::fs::write(
            cfg.backend.join("fake1.txt"),
            "  ✓ store with data set \"forbidden\"\n  ✓ store again with data set \"state:paid\"\n  ⨯ store again with data set \"state:closed\"\n   FAILED  Tests\\Feature\\Http\\Controllers\\OrderControllerTest > store again with data set \"state:closed\"\n  Expected 422 but received 200.\n\n  Tests: 1 failed, 2 passed\n",
        )
        .unwrap();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + "[tests]\nrun = \"cat fake{slot}.txt # {filter}\"\nparallel = 2\n";
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let mut s = server();
        let args = json!({"project": cfg.root, "tests": [
            {"target": "POST api/orders/{order}", "method": "test_store", "method_php": "public function test_store(int $s): void {}", "rows": [{"case": "forbidden", "args_php": "403"}]},
            {"target": "POST api/orders/{order}", "method": "test_store_again", "method_php": "public function test_store_again(int $s): void {}", "rows": [{"case": "state:paid", "args_php": "200"}, {"case": "state:closed", "args_php": "422"}]},
            {"target": "POST api/orders/{order}", "method": "test_store_x", "rows": [{"case": "guest", "args_php": "401"}]}
        ]});
        let (text, err) = call(&mut s, "write_test", args);
        assert!(!err, "{text}");
        assert!(text.contains("3 target(s): 2 written, 1 not written"), "{text}");
        assert!(
            text.contains("state:closed failed and was removed") && text.contains("Expected 422"),
            "{text}"
        );
        assert!(text.contains("already covered"), "{text}");
        let php = std::fs::read_to_string(
            cfg.backend
                .join("tests/Feature/Http/Controllers/OrderControllerTest.php"),
        )
        .unwrap();
        assert!(
            php.contains("'forbidden' => [403]") && php.contains("'state:paid' => [200]"),
            "{php}"
        );
        assert!(!php.contains("state:closed"), "{php}");
        assert!(State::load(&cfg).queue.is_empty());
    }

    #[test]
    fn write_test_rolls_back_when_no_row_ran() {
        let (_d, cfg) = project();
        std::fs::write(
            cfg.backend.join("fake.txt"),
            "  ✓ some other test\n\n  WARN  The data provider specified for test_store is invalid\n  Tests: 1 passed\n",
        )
        .unwrap();
        let toml = std::fs::read_to_string(cfg.root.join(".doctest.toml")).unwrap()
            + "[tests]\nrun = \"cat fake.txt # {filter}\"\n";
        std::fs::write(cfg.root.join(".doctest.toml"), toml).unwrap();
        let mut s = server();
        let args = json!({
            "project": cfg.root, "target": "POST api/orders/{order}", "method": "test_store",
            "method_php": "public function test_store(int $s): void {}",
            "rows": [{"case": "forbidden", "args_php": "__('x')"}]
        });
        let (text, err) = call(&mut s, "write_test", args);
        assert!(
            err && text.contains("no row of test_store ran") && text.contains("data provider"),
            "{text}"
        );
        let f = cfg
            .backend
            .join("tests/Feature/Http/Controllers/OrderControllerTest.php");
        assert!(!std::fs::read_to_string(&f).unwrap_or_default().contains("__('x')"));
    }

    #[test]
    fn write_test_skip_marks_a_target_untestable() {
        let (_d, cfg) = project();
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "write_test",
            json!({"project": cfg.root, "target": "POST api/orders/{order}", "skip": "calls exit()"}),
        );
        assert!(!err, "{text}");
        assert_eq!(State::load(&cfg).untestable["POST api/orders/{order}"], "calls exit()");
        let (text, _) = call(
            &mut s,
            "write_test",
            json!({"project": cfg.root, "target": "POST api/orders/{order}", "unskip": true}),
        );
        assert!(text.contains("testable again"), "{text}");
        assert!(State::load(&cfg).untestable.is_empty());
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
    fn module_facts_list_endpoints_and_tables_without_the_server_block() {
        let (_d, cfg) = project();
        let ctl = cfg.backend.join("app/Http/Controllers/OrderController.php");
        let src = std::fs::read_to_string(&ctl).unwrap().replacen(
            "use Illuminate\\Http\\Request;",
            "use Illuminate\\Http\\Request;\nuse App\\Models\\Order;",
            1,
        );
        assert!(src.contains("App\\Models\\Order"), "{src}");
        std::fs::write(&ctl, src).unwrap();
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "facts",
            json!({"project": cfg.root, "item": "doc:system/modules/orders"}),
        );
        assert!(!err, "{text}");
        let v: Value = serde_json::from_str(text.split("\n— doctest").next().unwrap()).unwrap();
        assert!(v["server"].is_null(), "server block repeated: {text}");
        let d = &v["details"];
        assert!(d["endpoints"].as_object().is_some_and(|e| !e.is_empty()), "{d}");
        assert!(d["controllers"].to_string().contains("OrderController"), "{d}");
        assert!(d["tables"].to_string().contains("\"orders\""), "{d}");
        let (index, _) = call(&mut s, "facts", json!({"project": cfg.root, "item": "doc:index"}));
        assert!(index.contains("\"server\": {"), "index keeps the server block");
    }

    #[test]
    fn translations_reuse_the_primary_screenshots() {
        let (_d, cfg) = project();
        let mut s = server();
        let md =
            "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\nClick **Save**.\n";
        call(
            &mut s,
            "write_doc",
            json!({"project": cfg.root, "item": "guide:en:shop.orders.create", "markdown": md}),
        );
        let (text, err) = call(
            &mut s,
            "facts",
            json!({"project": cfg.root, "item": "translation:id:shop.orders.create"}),
        );
        assert!(!err, "{text}");
        assert!(
            text.contains("reuse the en guide's screenshots") && !text.contains("screenshot_plan"),
            "{text}"
        );
        let (e, err) = call(
            &mut s,
            "shoot",
            json!({"project": cfg.root, "retake_page": "shop.orders.create", "lang": "id"}),
        );
        assert!(err && e.contains("no stored screenshots"), "{e}");
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
        // a slow machine may tick one more second between the calls
        assert!(
            !err && (text.contains("took 2m 5s") || text.contains("took 2m 6s")),
            "{text}"
        );
        let st = State::load(&cfg);
        let secs = st.runs.last().unwrap().seconds.unwrap();
        assert!((125..=126).contains(&secs), "{secs}");
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
    fn claim_keeps_guide_and_test_queues_apart() {
        let (_d, cfg) = project();
        let mut s = server();
        let ids = |t: &str| -> Vec<String> {
            serde_json::from_str::<Value>(t).unwrap()["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect()
        };
        let (t1, err) = call(
            &mut s,
            "claim",
            json!({"project": cfg.root, "worker": "tests1", "count": 50, "queue": "tests", "scope": "all"}),
        );
        assert!(!err, "{t1}");
        let tests = ids(&t1);
        assert!(
            !tests.is_empty() && tests.iter().all(|i| i.starts_with("tests:")),
            "{tests:?}"
        );
        // a guide worker never gets a test item, even though the queue is already full
        let (g, _) = call(
            &mut s,
            "claim",
            json!({"project": cfg.root, "worker": "g1", "count": 50, "queue": "guides"}),
        );
        let guides = ids(&g);
        assert!(
            !guides.is_empty() && guides.iter().all(|i| !i.starts_with("tests:")),
            "{guides:?}"
        );
        // a portal nobody has: no page items, only items of no portal
        let (p, _) = call(
            &mut s,
            "plan",
            json!({"project": cfg.root, "scope": "all", "portals": ["nowhere"], "limit": 500}),
        );
        assert!(!p.contains("guide:en:"), "{p}");
        let (bad, err) = call(
            &mut s,
            "claim",
            json!({"project": cfg.root, "worker": "x", "queue": "pages"}),
        );
        assert!(err && bad.contains("guides or tests"), "{bad}");
    }

    #[test]
    fn items_belong_to_their_page_portal() {
        let mut plan = work::Plan::default();
        let page = |key: &str, portal: &str| crate::pages::Page {
            key: key.into(),
            portal: portal.into(),
            ..Default::default()
        };
        plan.pages = vec![page("admin.users", "admin"), page("selfcare.home", "selfcare")];
        for id in [
            "guide:en:admin.users",
            "translation:ms:selfcare.home",
            "tests:GET api/contractor/jobs",
            "doc:index",
        ] {
            plan.items.push(work::WorkItem {
                id: id.into(),
                kind: work::ItemKind::Doc,
                reason: String::new(),
            });
        }
        let mut cfg = project().1;
        cfg.guide.portals = vec![crate::config::Portal {
            name: "contractor".into(),
            pages: vec!["/contractor/*".into()],
            ..Default::default()
        }];
        let ip = work::item_portals(&plan, &State::default(), &cfg);
        assert_eq!(ip["guide:en:admin.users"], vec!["admin"]);
        assert_eq!(ip["translation:ms:selfcare.home"], vec!["selfcare"]);
        assert_eq!(ip["tests:GET api/contractor/jobs"], vec!["contractor"]);
        let want = vec!["admin".to_string(), "selfcare".to_string()];
        assert!(work::item_wanted("guide:en:admin.users", &[], &want, &ip));
        assert!(!work::item_wanted("tests:GET api/contractor/jobs", &[], &want, &ip));
        assert!(work::item_wanted("doc:index", &[], &want, &ip));
    }

    #[test]
    fn unknown_project_is_tool_error() {
        let mut s = server();
        let (text, err) = call(&mut s, "status", json!({"project": "/nonexistent/x"}));
        assert!(err && text.contains("/nonexistent/x"), "{text}");
    }
}
