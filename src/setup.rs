//! `doctest setup`: makes doctest available to the AI app (Claude Code): registers the MCP server, writes a rule
//! block into CLAUDE.md, and (scope project) writes .doctest.toml when the Laravel repo has none. `--check` only
//! reports, `--dry-run` shows what would change, `--remove` undoes it. Mirrors `codemap setup` (without its hook:
//! doctest needs none).

use std::path::{Path, PathBuf};

pub const BEGIN: &str = "<!-- doctest:begin -->";
pub const END: &str = "<!-- doctest:end -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

pub struct Opts {
    pub scope: Scope,
    pub project: PathBuf,
    /// CLAUDE.md to write the rule block into (~/.claude/CLAUDE.md for user scope, <project>/CLAUDE.md for project).
    pub claude_md: PathBuf,
    pub exe: PathBuf,
    pub check: bool,
    pub remove: bool,
    pub dry_run: bool,
}

pub fn rule_block() -> String {
    "# doctest (user guide, system document, tests)
The `doctest` MCP server keeps a Laravel + Vue project's user guide (document/guide: one guide per portal and language,
with screenshots), its system document (document/index.html and system/) and its PHPUnit tests current. Use it
whenever the user asks to document or test the system, the latest changes or a branch.
- Load its tools first if they are deferred (in Claude Code, one ToolSearch `select:` call for the `mcp__doctest__` tools).
- `status` / `plan` (scope `changes`, `branch` or `all`) → for each item `facts` → `write_doc` / `shoot` / `write_test`.
  Write only what facts gives: never a field, button, endpoint or message it did not list.
- Tests: `cases` first; one parameterised method with a data provider per target; a refused or failing row is
  information — read the code, then fix the row or mark it as a bug.
- Parallel work: several workers each `claim` their own items.
- `issues` and `verify_changes` give code points to fix (file:line, reason, fix); tell the user about them.
- When doctest gives a wrong answer or fails, call `report_problem` (its own failures are recorded automatically) and
  tell the user the id: `doctest reports --send <id>` files it as a GitHub issue after they confirm.
- `check`, then `finish` when the work is done.
"
    .to_string()
}

/// `text` with the doctest block added at the end, or replaced where it is.
pub fn upsert_block(text: &str, block: &str) -> String {
    let wrapped = format!("{BEGIN}\n{}\n{END}", block.trim_end());
    match (text.find(BEGIN), text.find(END)) {
        (Some(s), Some(e)) if e > s => format!("{}{wrapped}{}", &text[..s], &text[e + END.len()..]),
        _ => {
            let base = text.trim_end();
            if base.is_empty() {
                format!("{wrapped}\n")
            } else {
                format!("{base}\n\n{wrapped}\n")
            }
        }
    }
}

pub fn remove_block(text: &str) -> String {
    match (text.find(BEGIN), text.find(END)) {
        (Some(s), Some(e)) if e > s => {
            let rest = format!("{}{}", text[..s].trim_end(), &text[e + END.len()..]);
            rest.trim_end().to_string() + "\n"
        }
        _ => text.to_string(),
    }
}

/// Runs the `claude` CLI; tests pass a fake.
pub type Claude<'a> = &'a dyn Fn(&[&str]) -> Result<String, String>;

pub fn claude_cli(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("claude")
        .args(args)
        .output()
        .map_err(|_| "the `claude` command was not found".to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn registered(claude: Claude) -> bool {
    claude(&["mcp", "get", "doctest"]).is_ok_and(|o| o.contains("doctest"))
}

pub fn run(o: &Opts, claude: Claude) -> Result<String, String> {
    let mut lines = Vec::new();
    let is_registered = registered(claude);
    let md = std::fs::read_to_string(&o.claude_md).unwrap_or_default();
    let has_block = md.contains(BEGIN);
    let config = crate::config::Config::find(&o.project).ok();
    if o.check {
        lines.push(format!(
            "MCP server: {}",
            if is_registered { "registered" } else { "not registered" }
        ));
        lines.push(format!(
            "rule block in {}: {}",
            o.claude_md.display(),
            if has_block { "present" } else { "missing" }
        ));
        lines.push(format!(
            ".doctest.toml: {}",
            config
                .as_ref()
                .map(|c| c.root.join(crate::config::FILE).display().to_string())
                .unwrap_or_else(|| "none (run `doctest init` in the Laravel repo)".into())
        ));
        return Ok(lines.join("\n"));
    }
    let would = if o.dry_run { "would " } else { "" };
    if o.remove {
        if is_registered {
            lines.push(format!("{would}remove the MCP server (scope {})", o.scope.flag()));
            if !o.dry_run {
                claude(&["mcp", "remove", "doctest", "-s", o.scope.flag()])?;
            }
        }
        if has_block {
            lines.push(format!("{would}remove the rule block from {}", o.claude_md.display()));
            if !o.dry_run {
                std::fs::write(&o.claude_md, remove_block(&md))
                    .map_err(|e| format!("cannot write {}: {e}", o.claude_md.display()))?;
            }
        }
        if lines.is_empty() {
            lines.push("nothing to remove".into());
        }
        return Ok(lines.join("\n"));
    }
    if is_registered {
        lines.push("MCP server: already registered".into());
    } else {
        let exe = o.exe.display().to_string();
        lines.push(format!(
            "{would}register the MCP server: claude mcp add doctest -s {} -- {exe} serve",
            o.scope.flag()
        ));
        if !o.dry_run {
            claude(&["mcp", "add", "doctest", "-s", o.scope.flag(), "--", &exe, "serve"]).map_err(|e| {
                format!(
                    "{e}\nRegister it by hand: claude mcp add doctest -s {} -- {exe} serve",
                    o.scope.flag()
                )
            })?;
        }
    }
    let new_md = upsert_block(&md, &rule_block());
    if new_md != md {
        lines.push(format!(
            "{would}{} the rule block in {}",
            if has_block { "update" } else { "add" },
            o.claude_md.display()
        ));
        if !o.dry_run {
            if let Some(dir) = o.claude_md.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&o.claude_md, new_md).map_err(|e| format!("cannot write {}: {e}", o.claude_md.display()))?;
        }
    } else {
        lines.push("rule block: up to date".into());
    }
    if o.scope == Scope::Project && config.is_none() {
        if o.project.join("composer.json").is_file() {
            lines.push(format!("{would}write .doctest.toml (doctest init)"));
            if !o.dry_run {
                let p = crate::init::run(&o.project)?;
                lines.push(format!("wrote {}; fill in its TODO lines", p.display()));
            }
        } else {
            lines.push(
                "no .doctest.toml: run `doctest setup --scope project` (or `doctest init`) in the Laravel repo".into(),
            );
        }
    }
    if !o.dry_run {
        let start = std::fs::canonicalize(&o.project).unwrap_or_else(|_| o.project.clone());
        let cfg = crate::config::Config::find(&start).ok();
        lines.push(String::new());
        lines.push(crate::doctor::render(&crate::doctor::run(cfg.as_ref(), &start)));
    }
    Ok(lines.join("\n"))
}

/// Default CLAUDE.md for a scope.
pub fn claude_md_for(scope: Scope, project: &Path) -> PathBuf {
    match scope {
        Scope::User => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude/CLAUDE.md"),
        Scope::Project => project.join("CLAUDE.md"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn block_is_added_once_and_replaced() {
        let once = upsert_block("# My rules\n", "BLOCK v1");
        assert!(once.starts_with("# My rules\n") && once.contains(&format!("{BEGIN}\nBLOCK v1\n{END}")));
        let twice = upsert_block(&once, "BLOCK v2");
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert!(twice.contains("BLOCK v2") && !twice.contains("BLOCK v1"));
        let gone = remove_block(&twice);
        assert!(!gone.contains(BEGIN) && gone.contains("# My rules"));
    }

    #[test]
    fn rule_block_names_the_workflow() {
        let b = rule_block();
        for want in [
            "facts",
            "write_doc",
            "write_test",
            "claim",
            "report_problem",
            "doctest reports --send",
            "finish",
        ] {
            assert!(b.contains(want), "{want} missing");
        }
        assert!(!b.to_lowercase().contains("irems"));
    }

    fn opts(dir: &std::path::Path, scope: Scope) -> Opts {
        Opts {
            scope,
            project: dir.to_path_buf(),
            claude_md: dir.join("CLAUDE.md"),
            exe: "/usr/local/bin/doctest".into(),
            check: false,
            remove: false,
            dry_run: false,
        }
    }

    #[test]
    fn project_setup_registers_writes_block_and_config() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("composer.json"), r#"{"name":"acme/shop"}"#).unwrap();
        let calls = RefCell::new(Vec::new());
        let claude = |args: &[&str]| -> Result<String, String> {
            calls.borrow_mut().push(args.join(" "));
            Ok(if args[1] == "get" { String::new() } else { "ok".into() })
        };
        let out = run(&opts(d.path(), Scope::Project), &claude).unwrap();
        assert!(
            calls
                .borrow()
                .iter()
                .any(|c| c == "mcp add doctest -s project -- /usr/local/bin/doctest serve"),
            "{:?}",
            calls.borrow()
        );
        assert!(
            std::fs::read_to_string(d.path().join("CLAUDE.md"))
                .unwrap()
                .contains(BEGIN)
        );
        assert!(d.path().join(".doctest.toml").is_file(), "{out}");
    }

    #[test]
    fn dry_run_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let claude = |args: &[&str]| -> Result<String, String> {
            if args[1] == "get" {
                Err("not found".into())
            } else {
                panic!("dry run ran {args:?}")
            }
        };
        let mut o = opts(d.path(), Scope::User);
        o.dry_run = true;
        let out = run(&o, &claude).unwrap();
        assert!(out.contains("would") && !d.path().join("CLAUDE.md").exists(), "{out}");
    }

    #[test]
    fn already_registered_is_not_added_again_and_remove_undoes() {
        let d = tempfile::tempdir().unwrap();
        let calls = RefCell::new(Vec::new());
        let claude = |args: &[&str]| -> Result<String, String> {
            calls.borrow_mut().push(args.join(" "));
            Ok("doctest: /usr/local/bin/doctest serve".into())
        };
        run(&opts(d.path(), Scope::User), &claude).unwrap();
        assert!(!calls.borrow().iter().any(|c| c.starts_with("mcp add")));
        let mut o = opts(d.path(), Scope::User);
        o.remove = true;
        run(&o, &claude).unwrap();
        assert!(calls.borrow().iter().any(|c| c == "mcp remove doctest -s user"));
        assert!(
            !std::fs::read_to_string(d.path().join("CLAUDE.md"))
                .unwrap()
                .contains(BEGIN)
        );
    }
}
