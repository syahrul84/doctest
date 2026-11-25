//! Problems with doctest itself, kept on this machine so doctest can be improved: reported by the AI through the
//! MCP tool `report_problem`, or found by doctest (a panic, a codemap failure, test output it could not read).
//! Nothing leaves the machine unless a person sends one (`doctest reports --send <id>`) and says yes; it is then
//! filed as a GitHub issue on the doctest repository (or added to the open issue for the same problem).
//!
//! What may be sent is scrubbed: no paths, hosts, emails, user names or long values; only doctest's version, the
//! OS, the step, a target pattern and the error with its details replaced by placeholders.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde::{Deserialize, Serialize};

pub const REPO: &str = "syahrul84/doctest";
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TARGETS: usize = 50;
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub id: String,
    /// panic, codemap-failure, unreadable-test-output, wrong-answer, confusing-answer, missing-feature …
    pub kind: String,
    /// "ai" or "auto"
    pub source: String,
    /// The MCP tool or command it happened in.
    pub step: String,
    pub summary: String,
    #[serde(default)]
    pub expected: Option<String>,
    #[serde(default)]
    pub actual: Option<String>,
    pub tool_version: String,
    pub os: String,
    pub count: u64,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(default)]
    pub sent: Option<String>,
    /// Item or target patterns it was seen on (`POST api/orders/{id}`, `guide:en:admin.x`).
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub versions: Vec<String>,
}

/// Where doctest keeps its own files (reports, update check): DOCTEST_HOME, else ~/.doctest.
pub fn tool_root() -> PathBuf {
    if let Ok(h) = std::env::var("DOCTEST_HOME") {
        return PathBuf::from(h);
    }
    // doctest's own tests never write into the user's reports, caches or Chrome profile
    if cfg!(test) {
        return std::env::temp_dir().join(format!("doctest-unit-tests-{}", std::process::id()));
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".doctest")
}

/// Paths, hosts, emails, long numbers and long quoted values reduced to placeholders.
pub fn scrub(text: &str) -> String {
    static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"[\w.+-]+@[\w-]+(\.[\w-]+)+", "<email>"),
            (r"https?://[^\s'\x22)]+", "<url>"),
            (r"\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b", "<ip>"),
            (r"(/[\w.@-]+){2,}/([\w.-]+\.(?:php|rs|vue|ts|js|json|md))", "…/$2"),
            (r"([A-Za-z]:)?(\\[\w.@ -]+){2,}\\([\w.-]+)", "…\\$3"),
            (r"(/[\w.@-]+){2,}", "<path>"),
            (r#"'[^']{24,}'|"[^"]{24,}""#, "'<text>'"),
            (r"\b\d{3,}\b", "<n>"),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).unwrap(), r))
        .collect()
    });
    let mut out = text.chars().take(600).collect::<String>();
    for (pattern, replacement) in RULES.iter() {
        out = pattern.replace_all(&out, *replacement).into_owned();
    }
    out
}

/// Errors that are doctest's (or its helpers') fault rather than a wrong request.
pub fn is_internal(error: &str) -> bool {
    let e = error.to_lowercase();
    [
        "panicked",
        "codemap stopped",
        "codemap did not answer",
        "codemap answer is not json",
        "cannot start codemap",
        "cannot start chrome",
        "screenshot failed",
        "bad screenshot",
        "ran no tests",
        "does not parse",
    ]
    .iter()
    .any(|m| e.contains(m))
}

/// A target pattern: numbers in a URI become {id}; long values are cut.
fn pattern(target: Option<&str>) -> Option<String> {
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/\d+(/|$)").unwrap());
    let t = target?.trim();
    if t.is_empty() {
        return None;
    }
    Some(DIGITS.replace_all(t, "/{id}$1").chars().take(200).collect())
}

/// The error a report is about, the same wherever it happened: first line, scrubbed, numbers as #.
fn error_key(summary: &str) -> String {
    static NUMBERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let line = summary.trim().lines().next().unwrap_or_default();
    let line: String = scrub(line).chars().take(160).collect();
    NUMBERS.replace_all(line.trim(), "#").into_owned()
}

fn push_unique(list: &mut Vec<String>, value: &str, max: usize) {
    if !value.is_empty() && !list.iter().any(|v| v == value) && list.len() < max {
        list.push(value.to_string());
    }
}

pub struct New<'a> {
    pub kind: &'a str,
    pub source: &'a str,
    pub step: &'a str,
    pub target: Option<&'a str>,
    pub summary: &'a str,
    pub expected: Option<&'a str>,
    pub actual: Option<&'a str>,
}

fn file_in(root: &Path) -> PathBuf {
    root.join("reports.jsonl")
}

/// Add a report, or count it again when the same problem was reported before. Returns its id.
pub fn add(new: New) -> String {
    add_at(&tool_root(), new)
}

pub fn add_at(root: &Path, new: New) -> String {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let summary = scrub(new.summary);
    let id = format!(
        "d_{}",
        &crate::hash::sha256(&format!("{}|{}|{}", new.kind, new.step, error_key(&summary)))[..8]
    );
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let version = crate::update::LONG_VERSION;
    let target = pattern(new.target);
    let mut all = load_at_unlocked(root);
    if let Some(r) = all.iter_mut().find(|r| r.id == id) {
        r.count += 1;
        r.last_seen = now;
        push_unique(&mut r.targets, target.as_deref().unwrap_or(""), MAX_TARGETS);
        push_unique(&mut r.versions, version, 20);
    } else {
        all.push(Report {
            id: id.clone(),
            kind: new.kind.into(),
            source: new.source.into(),
            step: new.step.into(),
            summary,
            expected: new.expected.map(scrub),
            actual: new.actual.map(scrub),
            tool_version: version.into(),
            os: std::env::consts::OS.into(),
            count: 1,
            first_seen: now.clone(),
            last_seen: now,
            sent: None,
            targets: target.into_iter().collect(),
            versions: vec![version.into()],
        });
    }
    save_at_unlocked(root, &all);
    id
}

fn load_at_unlocked(root: &Path) -> Vec<Report> {
    std::fs::read_to_string(file_in(root))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn save_at_unlocked(root: &Path, all: &[Report]) {
    let mut lines: Vec<String> = all.iter().filter_map(|r| serde_json::to_string(r).ok()).collect();
    while lines.iter().map(|l| l.len() as u64 + 1).sum::<u64>() > MAX_BYTES && lines.len() > 1 {
        lines.remove(0);
    }
    let _ = std::fs::create_dir_all(root);
    let _ = std::fs::write(file_in(root), lines.join("\n") + "\n");
}

pub fn load() -> Vec<Report> {
    load_at(&tool_root())
}

pub fn load_at(root: &Path) -> Vec<Report> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load_at_unlocked(root)
}

pub fn clear() -> usize {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let n = load_at_unlocked(&tool_root()).len();
    let _ = std::fs::remove_file(file_in(&tool_root()));
    n
}

/// What would be sent for a report: the issue title and body.
pub fn issue(r: &Report) -> (String, String) {
    let title = format!("[{}] {}", r.kind, r.summary.chars().take(90).collect::<String>());
    let mut body = format!(
        "**Kind:** {}\n**Found by:** {}\n**Step:** {}\n**Seen:** {} times ({} to {})\n**doctest:** {} on {}\n",
        r.kind, r.source, r.step, r.count, r.first_seen, r.last_seen, r.tool_version, r.os
    );
    if !r.targets.is_empty() {
        let shown: Vec<String> = r.targets.iter().take(10).map(|t| format!("`{t}`")).collect();
        body += &format!(
            "**Seen on:** {}{}\n",
            shown.join(", "),
            if r.targets.len() > 10 { ", …" } else { "" }
        );
    }
    if r.versions.len() > 1 {
        body += &format!("**doctest versions:** {}\n", r.versions.join(", "));
    }
    body += &format!("\n{}\n", r.summary);
    if let Some(e) = &r.expected {
        body += &format!("\n**Expected:** {e}\n");
    }
    if let Some(a) = &r.actual {
        body += &format!("**Actual:** {a}\n");
    }
    body += &format!("\n<!-- doctest-report: {} -->\n", r.id);
    (title, body)
}

fn mark_sent(id: &str, where_: &str) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tool_root();
    let mut all = load_at_unlocked(&root);
    if let Some(r) = all.iter_mut().find(|r| r.id == id) {
        r.sent = Some(where_.into());
    }
    save_at_unlocked(&root, &all);
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Lists reports for the terminal.
pub fn list() -> String {
    let all = load();
    if all.is_empty() {
        return "No problem reports. (doctest keeps them in ~/.doctest/reports.jsonl)".into();
    }
    let mut out = format!("{} problem report(s):\n", all.len());
    for r in &all {
        out += &format!(
            "  {}  [{}] {}× {} — {}{}\n",
            r.id,
            r.kind,
            r.count,
            r.step,
            r.summary.chars().take(100).collect::<String>(),
            r.sent.as_ref().map(|s| format!(" (sent: {s})")).unwrap_or_default()
        );
    }
    out + "Send one as a GitHub issue with `doctest reports --send <id>` (you see it first and confirm)."
}

/// Send one report as a GitHub issue after the person says yes: comment on the open issue for the same problem if
/// there is one, else open a new issue; without a signed-in `gh`, print a new-issue link.
pub fn send(id: &str) -> Result<String, String> {
    let report = load()
        .into_iter()
        .find(|r| r.id == id)
        .ok_or(format!("no report {id} (doctest reports lists them)"))?;
    let (title, body) = issue(&report);
    println!("This is exactly what would be sent to github.com/{REPO}:\n\n# {title}\n\n{body}");
    print!("Send it? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        return Ok("Not sent.".into());
    }
    let gh_ok = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !gh_ok {
        let link = format!(
            "https://github.com/{REPO}/issues/new?title={}&body={}",
            encode(&title),
            encode(&body)
        );
        return Ok(format!("gh is not signed in; open this link to file it:\n{link}"));
    }
    let marker = format!("doctest-report: {}", report.id);
    let existing = Command::new("gh")
        .args([
            "issue",
            "list",
            "-R",
            REPO,
            "--state",
            "open",
            "--search",
            &marker,
            "--json",
            "number",
            "-q",
            ".[0].number",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    let number = String::from_utf8_lossy(&existing.stdout).trim().to_string();
    let out = if number.is_empty() {
        Command::new("gh")
            .args(["issue", "create", "-R", REPO, "--title", &title, "--body", &body])
            .output()
    } else {
        Command::new("gh")
            .args(["issue", "comment", &number, "-R", REPO, "--body", &body])
            .output()
    }
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let link = String::from_utf8_lossy(&out.stdout).trim().to_string();
    mark_sent(id, &link);
    Ok(format!("Sent: {link}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing() {
        let s = scrub(
            "codemap get_symbol failed in /Users/bob/Documents/acme/acme-api/app/Http/Foo.php line 1234 for bob@x.com at http://10.0.0.1:8080/x",
        );
        assert!(s.contains("…/Foo.php"), "{s}");
        assert!(s.contains("<n>") && s.contains("<email>") && s.contains("<url>"), "{s}");
        assert!(!s.contains("bob") && !s.contains("acme"), "{s}");
    }

    #[test]
    fn the_same_problem_is_one_report_counted() {
        let d = tempfile::tempdir().unwrap();
        let a = add_at(
            d.path(),
            New {
                kind: "panic",
                source: "auto",
                step: "write_test",
                target: Some("POST api/orders/42"),
                summary: "index out of range at /a/b/c.rs:12",
                expected: None,
                actual: None,
            },
        );
        let b = add_at(
            d.path(),
            New {
                kind: "panic",
                source: "auto",
                step: "write_test",
                target: Some("PUT api/bills/7"),
                summary: "index out of range at /x/y/c.rs:99",
                expected: None,
                actual: None,
            },
        );
        assert_eq!(a, b);
        let all = load_at(d.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].count, 2);
        assert_eq!(all[0].targets, vec!["POST api/orders/{id}", "PUT api/bills/{id}"]);
        let (title, body) = issue(&all[0]);
        assert!(title.starts_with("[panic] "), "{title}");
        assert!(
            body.contains("**Step:** write_test") && body.contains(&format!("<!-- doctest-report: {a} -->")),
            "{body}"
        );
        assert!(
            !body.to_lowercase().contains("claude"),
            "no tool credit in issues: {body}"
        );
    }

    #[test]
    fn internal_errors_are_recognised() {
        assert!(is_internal("codemap stopped while answering tools/call"));
        assert!(is_internal("the tool facts panicked; please report it"));
        assert!(!is_internal("item is required"));
        assert!(!is_internal("already covered, not written: guest"));
    }
}
