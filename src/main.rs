//! doctest: keeps a Laravel + Vue project's user guide, system document and tests current, with Claude writing.

mod cases;
mod check;
mod codemap;
mod config;
mod doctor;
mod git;
mod hash;
mod html;
mod i18n;
mod init;
mod issues;
mod ledger;
mod mcp;
mod pages;
mod progress;
mod reports;
mod server;
mod setup;
mod shots;
mod source;
mod state;
mod testwrite;
mod update;
mod validate;
mod verify;
mod work;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "doctest", version = update::LONG_VERSION, about = "User guide, system document and tests for Laravel + Vue projects, kept current")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write .doctest.toml for the backend repo in the current folder
    Init {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// What is missing or stale: guides, translations, screenshots, docs, test cases
    Status {
        #[arg(long, default_value = ".")]
        project: PathBuf,
        /// `changes` (since the last finished run), `branch` (since the branch left --base) or `all`
        #[arg(long, default_value = "changes")]
        scope: String,
        /// Base branch for --scope branch (default origin/HEAD, main, master, develop)
        #[arg(long)]
        base: Option<String>,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Check the document folder and doctest's tests; exit 1 on problems (for CI and pre-commit)
    Check {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// Rebuild all HTML under the document folder from its sources
    Build {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// Validate the current changes: affected tests, doctest tests, stale guides, missing cases, code problems
    Verify {
        #[arg(long, default_value = ".")]
        project: PathBuf,
        /// `changes` (since the last finished run) or `branch`
        #[arg(long, default_value = "changes")]
        scope: String,
        #[arg(long)]
        base: Option<String>,
    },
    /// Problems with doctest itself, kept on this machine; send one as a GitHub issue
    Reports {
        /// Send this report (you see it first and confirm)
        #[arg(long)]
        send: Option<String>,
        /// Delete all reports
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install the latest release of doctest (checked against its SHA256SUMS)
    Update {
        /// Only say whether a newer version exists
        #[arg(long)]
        check: bool,
        /// Install the latest release even if it is not newer (a repair)
        #[arg(long)]
        force: bool,
    },
    /// Make doctest available to Claude Code: register the MCP server, add a rule block to CLAUDE.md, write
    /// .doctest.toml (scope project)
    Setup {
        /// `user` (every project, the default) or `project` (this folder)
        #[arg(long, default_value = "user")]
        scope: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
        /// Only report what is installed
        #[arg(long)]
        check: bool,
        /// Undo it: rule block and MCP server entry
        #[arg(long)]
        remove: bool,
        /// Show what would change and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Check what doctest needs and what to fix
    Doctor {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// Serve the MCP tools on stdio
    Serve {
        /// Tool calls that can run at the same time
        #[arg(long, default_value_t = 4)]
        workers: usize,
    },
}

fn load(project: &std::path::Path) -> Result<config::Config, String> {
    let start = std::fs::canonicalize(project).map_err(|e| format!("{}: {e}", project.display()))?;
    config::Config::find(&start)
}

fn scope_of(s: &str, base: Option<&str>) -> Result<work::Scope, String> {
    work::Scope::parse(Some(s), base)
}

fn status_cmd(project: &std::path::Path, scope: &str, base: Option<&str>, limit: usize) -> Result<String, String> {
    let cfg = load(project)?;
    let scope = scope_of(scope, base)?;
    let mut st = state::State::load(&cfg);
    let mut api = codemap::LiveCodemap::new(&cfg);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let make = || -> Box<dyn codemap::CodemapApi + Send> {
        let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % cfg.workers + 1;
        Box::new(codemap::LiveCodemap::worker(&cfg, n))
    };
    let plan = work::plan_parallel(
        &cfg,
        &mut api,
        &mut st,
        &scope,
        &progress::Stderr::default(),
        Some(&make),
    )?;
    state::State::update(&cfg, |s| s.merge_plan(&st))?;
    let s = work::status(&plan, &scope, limit);
    let c = &s.progress;
    let mut out = format!(
        "progress: guides {}/{} ({}%), translations {}/{} ({}%), screenshots {}/{} ({}%), test cases {}/{} ({}%)\n",
        c.guides.done,
        c.guides.total,
        c.guides.percent,
        c.translations.done,
        c.translations.total,
        c.translations.percent,
        c.screenshots.done,
        c.screenshots.total,
        c.screenshots.percent,
        c.test_cases.done,
        c.test_cases.total,
        c.test_cases.percent
    );
    out += &format!("doctest status ({}): ", s.scope);
    out += &if s.counts.is_empty() {
        "nothing to do".to_string()
    } else {
        s.counts
            .iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if s.stale_index {
        out += "\n(codemap's index was stale for some answers; it re-scans on the next call)";
    }
    for i in &s.items {
        out += &format!("\n  {}  — {}", i.id, i.reason);
    }
    if s.more > 0 {
        out += &format!("\n  … and {} more (use --limit)", s.more);
    }
    Ok(out)
}

fn check_cmd(project: &std::path::Path) -> Result<(String, bool), String> {
    let cfg = load(project)?;
    let mut st = state::State::load(&cfg);
    let mut api = codemap::LiveCodemap::new(&cfg);
    let r = check::run(&cfg, &mut api, &mut st, &progress::Stderr::default())?;
    let mut out = String::new();
    let section = |out: &mut String, title: &str, items: &[String]| {
        if !items.is_empty() {
            *out += &format!("{title} ({}):\n", items.len());
            for i in items {
                *out += &format!("  - {i}\n");
            }
        }
    };
    section(&mut out, "Errors", &r.errors);
    section(&mut out, "Warnings", &r.warnings);
    section(&mut out, "Known bugs (tests kept in group doctest-bug)", &r.bugs);
    let points: Vec<String> = r.points.iter().map(issues::Issue::point).collect();
    section(&mut out, "Code to fix", &points);
    out += if r.passed() { "check passed" } else { "check failed" };
    Ok((out, r.passed()))
}

fn verify_cmd(project: &std::path::Path, scope: &str, base: Option<&str>) -> Result<(String, bool), String> {
    let cfg = load(project)?;
    let mut st = state::State::load(&cfg);
    let mut api = codemap::LiveCodemap::new(&cfg);
    let scope = scope_of(scope, base)?;
    let since = match scope {
        work::Scope::Branch { .. } => work::since_of(&cfg, &st, &scope)?,
        work::Scope::All => return Err("verify takes scope changes or branch".into()),
        _ => None,
    };
    let r = verify::run(&cfg, &mut api, &mut st, since, &progress::Stderr::default())?;
    state::State::update(&cfg, |s| s.merge_plan(&st))?;
    let mut out = String::new();
    if !r.tests_run.is_empty() {
        out += &format!("Tests run: {}\n", r.tests_run.join(", "));
    }
    if !r.errors.is_empty() {
        out += &format!("To fix ({}):\n", r.errors.len());
        for e in &r.errors {
            out += &format!("  - {}\n", e.point());
        }
    }
    if !r.todo.is_empty() {
        out += &format!("Still to document or test ({}):\n", r.todo.len());
        for i in &r.todo {
            out += &format!("  - {}  — {}\n", i.id, i.reason);
        }
    }
    out += if r.pass { "verify passed" } else { "verify failed" };
    Ok((out, r.pass))
}

fn build_cmd(project: &std::path::Path) -> Result<String, String> {
    let cfg = load(project)?;
    let sources = source::sources(&cfg)?;
    let st = state::State::load(&cfg);
    let r = html::build(&cfg, &sources, &st)?;
    Ok(format!(
        "built {} files in {}{}",
        r.written.len(),
        cfg.out.display(),
        if r.removed.is_empty() {
            String::new()
        } else {
            format!("; removed {}", r.removed.len())
        }
    ))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let step = format!("cli:{}", std::env::args().nth(1).unwrap_or_default());
    let started = std::time::Instant::now();
    let result: Result<(String, bool), String> = match cli.command {
        Command::Status {
            project,
            scope,
            base,
            limit,
        } => status_cmd(&project, &scope, base.as_deref(), limit).map(|t| (t, true)),
        Command::Check { project } => check_cmd(&project),
        Command::Build { project } => build_cmd(&project).map(|t| (t, true)),
        Command::Verify { project, scope, base } => verify_cmd(&project, &scope, base.as_deref()),
        Command::Reports { send, clear, json } => {
            if let Some(id) = send {
                reports::send(&id).map(|t| (t, true))
            } else if clear {
                Ok((format!("removed {} report(s)", reports::clear()), true))
            } else if json {
                Ok((serde_json::to_string_pretty(&reports::load()).unwrap_or_default(), true))
            } else {
                Ok((reports::list(), true))
            }
        }
        Command::Update { check, force } => update::run(check, force).map(|t| (t, true)),
        Command::Setup {
            scope,
            project,
            check,
            remove,
            dry_run,
        } => (|| {
            let scope = match scope.as_str() {
                "user" => setup::Scope::User,
                "project" => setup::Scope::Project,
                other => return Err(format!("scope must be user or project, not {other}")),
            };
            let project = std::fs::canonicalize(&project).map_err(|e| format!("{}: {e}", project.display()))?;
            let exe = std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .map_err(|e| e.to_string())?;
            let opts = setup::Opts {
                scope,
                claude_md: setup::claude_md_for(scope, &project),
                project,
                exe,
                check,
                remove,
                dry_run,
            };
            setup::run(&opts, &setup::claude_cli).map(|t| (t, true))
        })(),
        Command::Doctor { project } => {
            let start = std::fs::canonicalize(&project).unwrap_or(project);
            let cfg = config::Config::find(&start).ok();
            let checks = doctor::run(cfg.as_ref(), &start);
            let ok = checks.iter().all(|c| c.status != doctor::Status::Fail);
            Ok((doctor::render(&checks), ok))
        }
        Command::Init { project } => {
            init::run(&project).map(|p| (format!("wrote {}; fill in the TODO lines", p.display()), true))
        }
        Command::Serve { workers } => {
            mcp::serve(workers);
            return ExitCode::SUCCESS;
        }
    };
    match result {
        Ok((text, ok)) => {
            println!("{text}");
            if !matches!(
                std::env::args().nth(1).as_deref(),
                Some("serve") | Some("--version") | Some("-V") | Some("help") | Some("--help")
            ) {
                eprintln!(
                    "(doctest {} took {:.1}s)",
                    step.trim_start_matches("cli:"),
                    started.elapsed().as_secs_f64()
                );
            }
            if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("doctest: {e}");
            if reports::is_internal(&e) {
                let id = reports::add(reports::New {
                    kind: "internal-error",
                    source: "auto",
                    step: &step,
                    target: None,
                    summary: &e,
                    expected: None,
                    actual: None,
                });
                eprintln!("(recorded as problem report {id}; file it with `doctest reports --send {id}`)");
            }
            ExitCode::FAILURE
        }
    }
}
