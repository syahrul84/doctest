//! Writes doctest's PHPUnit tests: per target one parameterised method and a data provider whose rows are keyed by
//! case, inside a `// <doctest>` block at the end of the test class; runs them and reads the results back.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const START: &str = "// <doctest>";
pub const END: &str = "// </doctest>";
const DP: &str = "\\PHPUnit\\Framework\\Attributes\\DataProvider";
const GROUP: &str = "\\PHPUnit\\Framework\\Attributes\\Group";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub case: String,
    /// The provider row's values, PHP, without the surrounding brackets: `true, ['email' => 'x'], 422`.
    pub args_php: String,
    #[serde(default)]
    pub bug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewTests {
    pub method: String,
    /// The parameterised method (signature and body). Leave out to add rows to an existing doctest method.
    #[serde(default)]
    pub method_php: Option<String>,
    pub rows: Vec<Row>,
}

/// Test file, class and namespace for a target: an existing `<Class>Test.php` under the feature dir if there is
/// one, else one mirroring `app/` (controllers → feature dir, other classes → unit dir).
pub fn target_file(cfg: &Config, _target: &str, action: &str) -> (PathBuf, String, String) {
    let (class_fqn, feature) = match action.split_once('@') {
        Some((c, _)) => (c, true),
        None => (action.split_once("::").map(|(c, _)| c).unwrap_or(action), false),
    };
    let short = class_fqn.rsplit('\\').next().unwrap_or(class_fqn);
    let test_class = format!("{short}Test");
    let dir = if feature {
        &cfg.tests.feature_dir
    } else {
        &cfg.tests.unit_dir
    };
    if feature {
        let mut found = Vec::new();
        let base = cfg.backend.join(dir);
        find_named(&base, &format!("{test_class}.php"), &mut found);
        found.sort();
        // the controller's folders under Controllers (Admin\Asset), to tell same-named controllers apart
        let want: Vec<String> = class_fqn
            .split('\\')
            .skip_while(|p| *p != "Controllers")
            .skip(1)
            .collect::<Vec<_>>()
            .split_last()
            .map(|(_, dirs)| dirs.iter().map(|d| d.to_lowercase()).collect())
            .unwrap_or_default();
        let dirs_of = |f: &Path| -> Vec<String> {
            f.parent()
                .and_then(|d| d.strip_prefix(&base).ok())
                .map(|d| {
                    d.components()
                        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                        .filter(|c| c != "http" && c != "controllers")
                        .collect()
                })
                .unwrap_or_default()
        };
        let exact = found.iter().find(|f| dirs_of(f) == want).cloned();
        // a test found elsewhere is only this controller's when no other controller has the same short name
        let pick = exact.or_else(|| {
            let mut same = Vec::new();
            find_named(&cfg.backend.join("app"), &format!("{short}.php"), &mut same);
            if same.len() > 1 { None } else { found.first().cloned() }
        });
        if let Some(f) = pick {
            let ns = std::fs::read_to_string(&f)
                .ok()
                .and_then(|t| NAMESPACE.captures(&t).map(|c| c[1].to_string()))
                .unwrap_or_else(|| namespace_for(cfg, f.parent().unwrap_or(&f)));
            return (f, test_class, ns);
        }
    }
    let rel = class_fqn.strip_prefix("App\\").unwrap_or(class_fqn);
    let mut parts: Vec<&str> = rel.split('\\').collect();
    parts.pop();
    let mut folder = cfg.backend.join(dir);
    for p in &parts {
        folder.push(p);
    }
    let ns = namespace_for(cfg, &folder);
    (folder.join(format!("{test_class}.php")), test_class, ns)
}

static NAMESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^namespace\s+([^;]+);").unwrap());

fn namespace_for(cfg: &Config, folder: &Path) -> String {
    let rel = folder.strip_prefix(&cfg.backend).unwrap_or(folder);
    rel.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            let mut ch = s.chars();
            ch.next()
                .map(|f| f.to_uppercase().collect::<String>() + ch.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\\")
}

fn find_named(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            find_named(&p, name, out);
        } else if e.file_name() == name {
            out.push(p);
        }
    }
}

static FUNC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"function\s+(\w+)\s*\(").unwrap());
static ROW_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*'((?:[^'\\]|\\.)*)'\s*=>").unwrap());

fn php_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// For each byte of PHP `text`: true when it is code (not inside a string, comment, heredoc or nowdoc).
pub fn php_code_mask(text: &str) -> Vec<bool> {
    let b = text.as_bytes();
    let mut mask = vec![true; b.len()];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        let start = i;
        if c == b'/' && next == Some(b'/') || c == b'#' && next != Some(b'[') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && next == Some(b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
        } else if b[i..].starts_with(b"<<<") {
            let rest = &text[i + 3..];
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let label: String = rest[..line_end]
                .trim()
                .trim_matches(['\'', '"'])
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if label.is_empty() {
                i += 3;
                continue;
            }
            i += 3 + line_end;
            // until a line whose trimmed start is the label followed by a non-identifier character
            loop {
                if i >= b.len() {
                    break;
                }
                let ls = i + 1;
                let le = text[ls.min(b.len())..].find('\n').map(|x| ls + x).unwrap_or(b.len());
                let line = text.get(ls..le).unwrap_or("").trim_start();
                if line.starts_with(&label)
                    && !line[label.len()..].starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
                {
                    i = ls + (text[ls..le].len() - line.len()) + label.len();
                    break;
                }
                i = le;
            }
        } else {
            i += 1;
            continue;
        }
        for m in mask.iter_mut().take(i.min(b.len())).skip(start) {
            *m = false;
        }
    }
    mask
}

/// Byte span of function `name` (from its docblock/attributes to after its closing brace and newline).
pub fn function_span(text: &str, name: &str) -> Option<(usize, usize)> {
    function_span_masked(text, &php_code_mask(text), name)
}

/// Spans of every function in `text`, scanning the PHP once (for files with many tests).
pub fn all_function_spans(text: &str) -> std::collections::HashMap<String, (usize, usize)> {
    let mask = php_code_mask(text);
    let mut out = std::collections::HashMap::new();
    for c in FUNC.captures_iter(text) {
        let name = c[1].to_string();
        if out.contains_key(&name)
            || !mask
                .get(c.get(0).map(|m| m.start()).unwrap_or(0))
                .copied()
                .unwrap_or(false)
        {
            continue;
        }
        if let Some(span) = function_span_masked(text, &mask, &name) {
            out.insert(name, span);
        }
    }
    out
}

fn function_span_masked(text: &str, mask: &[bool], name: &str) -> Option<(usize, usize)> {
    let re = Regex::new(&format!(r"function\s+{}\s*\(", regex::escape(name))).ok()?;
    let m = re
        .find_iter(text)
        .find(|m| mask.get(m.start()).copied().unwrap_or(false))?;
    let bytes = text.as_bytes();
    let open = (m.end()..bytes.len()).find(|&i| bytes[i] == b'{' && mask[i])?;
    let mut depth = 0i32;
    let mut end = None;
    for i in open..bytes.len() {
        if !mask[i] {
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let mut end = end?;
    if text[end..].starts_with('\n') {
        end += 1;
    }
    // walk back over the line start, docblock, attributes and one blank line before
    let mut start = text[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
    loop {
        let before = &text[..start];
        let Some(prev_nl) = before[..before.len().saturating_sub(1)].rfind('\n') else {
            break;
        };
        let line = before[prev_nl + 1..].trim();
        if line.starts_with("/**") || line.starts_with('*') || line.starts_with("#[") {
            start = prev_nl + 1;
        } else {
            break;
        }
    }
    let before = &text[..start];
    if before.ends_with("\n\n") {
        start -= 1;
    }
    Some((start, end))
}

static CLASS_DECL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:final\s+|abstract\s+|readonly\s+)*class\s+(\w+)").unwrap());
static PUBLIC_FN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*((?:(?:final|abstract|static|public)\s+)*)function\s+(\w+)\s*\(").unwrap()
});
static NAMESPACE_DECL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^namespace\s+([^;\s]+);").unwrap());

/// A PHP class file's class name (with namespace) and its public methods with their source (no constructor or
/// magic methods, no abstract ones), read without codemap.
pub fn public_methods(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let mask = php_code_mask(text);
    let class = CLASS_DECL
        .captures_iter(text)
        .find(|c| c.get(1).is_some_and(|m| mask.get(m.start()).copied().unwrap_or(false)))?;
    let fqn = match NAMESPACE_DECL.captures(text) {
        Some(n) => format!("{}\\{}", &n[1], &class[1]),
        None => class[1].to_string(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for c in PUBLIC_FN.captures_iter(text) {
        let name = c[2].to_string();
        let mods = &c[1];
        let at = c.get(2).map(|m| m.start()).unwrap_or(0);
        if !mask.get(at).copied().unwrap_or(false) || !mods.contains("public") || mods.contains("abstract") {
            continue;
        }
        if name.starts_with("__") || out.iter().any(|(n, _)| *n == name) {
            continue;
        }
        let after = &text[c.get(0).map(|m| m.end()).unwrap_or(0)..];
        let brace = after.find('{');
        let semi = after.find(';');
        if brace.is_none() || semi.is_some_and(|s| Some(s) < brace) {
            continue;
        }
        if let Some((s, e)) = function_span(text, &name) {
            out.push((name, text[s..e].to_string()));
        }
    }
    Some((fqn, out))
}

/// `[` minus `]` outside strings in one line of PHP.
fn bracket_delta(line: &str) -> i32 {
    let mask = php_code_mask(line);
    line.bytes()
        .zip(mask)
        .filter(|(_, m)| *m)
        .map(|(c, _)| match c {
            b'[' => 1,
            b']' => -1,
            _ => 0,
        })
        .sum()
}

/// A lock on one test file, so parallel workers do not overwrite each other's edits.
pub fn file_lock(file: &Path) -> Result<crate::state::FileLock, String> {
    // one key per file however its path is spelled (/var vs /private/var, relative parts)
    let resolved = match (
        file.parent().and_then(|d| std::fs::canonicalize(d).ok()),
        file.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => file.to_path_buf(),
    };
    let key = crate::hash::sha256(&resolved.to_string_lossy());
    // held while the write waits for the test runner and runs: other workers' runs can take far longer than the
    // default two minutes, after which the lock would be taken over and the edits clobbered
    crate::state::FileLock::acquire_with(std::env::temp_dir().join(format!("doctest-{}.lock", &key[..16])), 3600)
}

/// Writes a test file whole or not at all (a temp file renamed over it), so a test run never loads half a file.
pub fn write_atomic(file: &Path, text: &str) -> Result<(), String> {
    let dir = file.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // not named *Test.php, so PHPUnit never picks the temp file up
    let tmp = dir.join(format!(".{name}.{}.doctest-tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", file.display())
    })
}

static PROVIDER_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*#\[\s*\\?(?:[\w\\]*\\)?DataProvider\s*\([^\n]*\)\s*\][ \t]*\r?\n?").unwrap()
});

/// `method_php` without data-provider attributes (doctest writes the provider `cases_<method>` itself); refused
/// when it holds more than the one method.
fn sanitize_method(code: &str, method: &str) -> Result<String, String> {
    let code = PROVIDER_ATTR.replace_all(code, "").into_owned();
    let mask = php_code_mask(&code);
    let names: Vec<String> = FUNC
        .captures_iter(&code)
        .filter(|c| c.get(0).is_some_and(|m| mask.get(m.start()).copied().unwrap_or(false)))
        .map(|c| c[1].to_string())
        .collect();
    if names.len() > 1 {
        return Err(format!(
            "method_php holds {} functions ({}): send only public function {method}(...); doctest writes its data provider cases_{method} from rows itself",
            names.len(),
            names.join(", ")
        ));
    }
    Ok(code)
}

/// Lines of a docblock without `@dataProvider` (doctest names its own provider).
fn without_provider_lines(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.starts_with("@dataProvider")).collect()
}

/// Removes these rows (and their bug notes and continuation lines) from provider `name`.
fn drop_rows(text: &str, name: &str, cases: &[String]) -> String {
    let Some((s, e)) = function_span(text, name) else {
        return text.to_string();
    };
    let mut kept = Vec::new();
    let mut lines = text[s..e].lines().peekable();
    let mut pending_note: Option<String> = None;
    while let Some(line) = lines.next() {
        if line.trim().starts_with("// @doctest-bug ") {
            pending_note = Some(line.to_string());
            continue;
        }
        if let Some(c) = ROW_KEY.captures(line)
            && cases.contains(&c[1].replace("\\'", "'"))
        {
            pending_note = None;
            let mut depth: i32 = bracket_delta(line);
            while depth > 0 {
                let Some(next) = lines.next() else { break };
                depth += bracket_delta(next);
            }
            continue;
        }
        if let Some(n) = pending_note.take() {
            kept.push(n);
        }
        kept.push(line.to_string());
    }
    let mut replacement = kept.join("\n");
    if text[s..e].ends_with('\n') {
        replacement.push('\n');
    }
    let mut out = text.to_string();
    out.replace_range(s..e, &replacement);
    out
}

/// Makes method `m` a test (public, with its provider attribute) or a helper only its `_known_bugs` test calls
/// (protected: a method with no rows of its own would be an empty data set, which PHPUnit reports as an error).
fn set_test(text: &str, m: &str, test: bool) -> String {
    let Some((s, e)) = function_span(text, m) else {
        return text.to_string();
    };
    let span = &text[s..e];
    let sig = Regex::new(&format!(
        r"(?m)^([ \t]*)(?:public|protected|private)?\s*function\s+{}\s*\(",
        regex::escape(m)
    ))
    .unwrap();
    let Some(c) = sig.captures(span) else {
        return text.to_string();
    };
    let whole = c.get(0).expect("match");
    let indent = c[1].to_string();
    let attr = format!("{indent}#[{DP}('cases_{m}')]\n");
    let head = &span[..whole.start()];
    let mut head = head.replace(&attr, "");
    let rest = &span[whole.end()..];
    let new_sig = if test {
        head += &attr;
        format!("{indent}public function {m}(")
    } else {
        format!("{indent}protected function {m}(")
    };
    let mut out = text.to_string();
    out.replace_range(s..e, &format!("{head}{new_sig}{rest}"));
    out
}

/// After rows changed: no rows left → the method and its providers go; only bug rows → the method becomes a
/// helper; no bug rows → the bug section goes. Only functions inside the doctest block are touched.
fn settle_method(mut text: String, m: &str) -> String {
    let in_block = |text: &str, name: &str| -> bool {
        match (text.find(START), text.find(END), function_span(text, name)) {
            (Some(s), Some(e), Some((fs, fe))) => fs >= s && fe <= e,
            _ => false,
        }
    };
    let remove_in_block = |text: &mut String, name: &str| {
        if in_block(text, name)
            && let Some((s, e)) = function_span(text, name)
        {
            text.replace_range(s..e, "");
        }
    };
    let bugs = provider_rows(&text, &format!("bugs_{m}")).len();
    let main = provider_rows(&text, &format!("cases_{m}")).len();
    if bugs == 0 {
        for f in [format!("{m}_known_bugs"), format!("bugs_{m}")] {
            remove_in_block(&mut text, &f);
        }
    }
    if main == 0 && bugs == 0 {
        for f in [m.to_string(), format!("cases_{m}")] {
            remove_in_block(&mut text, &f);
        }
    } else if in_block(&text, m) {
        text = set_test(&text, m, main > 0);
    }
    let tidy = Regex::new(&format!(r"{}\n\s*\n(\s*{})", regex::escape(START), regex::escape(END))).unwrap();
    tidy.replace(&text, format!("{START}\n$1")).into_owned()
}

/// Row keys of provider `name` in `text`, with the bug note of each row.
pub fn provider_rows(text: &str, name: &str) -> Vec<(String, Option<String>)> {
    let Some((s, e)) = function_span(text, name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut note: Option<String> = None;
    for line in text[s..e].lines() {
        let t = line.trim();
        if let Some(n) = t.strip_prefix("// @doctest-bug ") {
            note = Some(n.to_string());
        } else if let Some(c) = ROW_KEY.captures(line) {
            out.push((c[1].replace("\\'", "'"), note.take()));
        }
    }
    out
}

fn reindent(code: &str, indent: usize) -> String {
    let lead = |l: &str| l.chars().take_while(|c| c.is_whitespace()).count();
    let min = code
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(lead)
        .min()
        .unwrap_or(0);
    code.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", " ".repeat(indent), l.chars().skip(min).collect::<String>())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_rows(rows: &[&Row], with_notes: bool) -> String {
    let mut s = String::new();
    for r in rows {
        if with_notes && let Some(n) = &r.bug {
            s += &format!("            // @doctest-bug {}\n", n.replace('\n', " "));
        }
        let args = r.args_php.trim().replace('\n', "\n            ");
        s += &format!("            '{}' => [{}],\n", php_str(&r.case), args);
    }
    s
}

fn provider(name: &str, rows: &str) -> String {
    format!("    public static function {name}(): array\n    {{\n        return [\n{rows}        ];\n    }}\n")
}

fn bug_section(target: &str, m: &str, rows: &str) -> String {
    format!(
        "\n    /**\n     * @doctest-bugs {target}\n     * @dataProvider bugs_{m}\n     * @group doctest-bug\n     */\n    #[{DP}('bugs_{m}')]\n    #[{GROUP}('doctest-bug')]\n    public function {m}_known_bugs(...$args): void\n    {{\n        $this->{m}(...$args);\n    }}\n\n{}",
        provider(&format!("bugs_{m}"), rows)
    )
}

/// Adds rows (and the method, the first time) for `target` to the doctest block of `file`.
#[cfg(test)]
pub fn insert(file: &Path, class: &str, namespace: &str, target: &str, t: &NewTests) -> Result<(), String> {
    let _lock = file_lock(file)?;
    insert_unlocked(file, class, namespace, target, t)
}

/// `insert` for a caller that already holds `file_lock(file)`.
pub fn insert_unlocked(file: &Path, class: &str, namespace: &str, target: &str, t: &NewTests) -> Result<(), String> {
    let m = t.method.as_str();
    if !m.starts_with("test") || !m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "method name {m} must start with test and use letters, digits and _ only"
        ));
    }
    if t.rows.is_empty() {
        return Err("no rows given".into());
    }
    for r in &t.rows {
        if !crate::cases::valid(&r.case) {
            return Err(format!(
                "'{}' is not a case key (ok, guest, forbidden, not_found, invalid:<field>:<rule>, boundary:<field>:min|max, filter:<param>, state:<name>, throws:<Exception>, returns:<name>)",
                r.case
            ));
        }
    }
    let mut text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(_) => format!(
            "<?php\n\nnamespace {namespace};\n\nuse Tests\\TestCase;\n\nclass {class} extends TestCase\n{{\n    {START}\n    {END}\n}}\n"
        ),
    };
    if !text.contains(START) {
        let close = text
            .rfind('}')
            .ok_or_else(|| format!("{}: no closing brace for the class", file.display()))?;
        let mut head = text[..close].trim_end().to_string();
        head += &format!("\n\n    {START}\n    {END}\n");
        text = head + &text[close..];
    }
    let block_start = text.find(START).expect("block exists");
    let block_end = text
        .find(END)
        .ok_or_else(|| format!("{}: {START} without {END}", file.display()))?;
    let block = &text[block_start..block_end];
    let ours = block.contains(&format!("function cases_{m}("));
    let main_rows: Vec<String> = provider_rows(block, &format!("cases_{m}"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let bug_rows: Vec<String> = provider_rows(block, &format!("bugs_{m}"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    // a row sent again with its bug note added or taken away moves between the two providers
    let to_bugs: Vec<String> = t
        .rows
        .iter()
        .filter(|r| r.bug.is_some() && main_rows.contains(&r.case))
        .map(|r| r.case.clone())
        .collect();
    let to_main: Vec<String> = t
        .rows
        .iter()
        .filter(|r| r.bug.is_none() && bug_rows.contains(&r.case))
        .map(|r| r.case.clone())
        .collect();
    if !to_bugs.is_empty() {
        text = drop_rows(&text, &format!("cases_{m}"), &to_bugs);
    }
    if !to_main.is_empty() {
        text = drop_rows(&text, &format!("bugs_{m}"), &to_main);
    }
    let existing: Vec<String> = main_rows
        .iter()
        .chain(&bug_rows)
        .filter(|k| !to_bugs.contains(k) && !to_main.contains(k))
        .cloned()
        .collect();
    let block_end = text.find(END).unwrap_or(block_end);
    for r in &t.rows {
        if existing.contains(&r.case) || t.rows.iter().filter(|x| x.case == r.case).count() > 1 {
            return Err(format!(
                "case '{}' already has a row in {m}; a case is tested once",
                r.case
            ));
        }
    }
    let normal: Vec<&Row> = t.rows.iter().filter(|r| r.bug.is_none()).collect();
    let bugs: Vec<&Row> = t.rows.iter().filter(|r| r.bug.is_some()).collect();

    if ours {
        if t.method_php.is_some() {
            return Err(format!(
                "{m} already exists in the doctest block; leave method_php out to add rows"
            ));
        }
        if !normal.is_empty() {
            text = add_rows(&text, &format!("cases_{m}"), &render_rows(&normal, false))?;
        }
        if !bugs.is_empty() {
            if text.contains(&format!("function bugs_{m}(")) {
                text = add_rows(&text, &format!("bugs_{m}"), &render_rows(&bugs, true))?;
            } else {
                let (_, end) = function_span(&text, &format!("cases_{m}")).expect("provider exists");
                text.insert_str(end, &bug_section(target, m, &render_rows(&bugs, true)));
            }
        }
        text = settle_method(text, m);
    } else {
        if FUNC.captures_iter(&text).any(|c| &c[1] == m) {
            return Err(format!(
                "{} already has a method {m}; pick another name",
                file.display()
            ));
        }
        let php = t
            .method_php
            .as_deref()
            .ok_or_else(|| format!("{m} is new: method_php is needed"))?
            .trim();
        let (own_doc, code) = match php.strip_prefix("/**") {
            Some(rest) => {
                let end = rest.find("*/").ok_or("method_php docblock is not closed")?;
                let lines: Vec<String> = rest[..end]
                    .lines()
                    .map(|l| l.trim().trim_start_matches('*').trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                (without_provider_lines(lines), rest[end + 2..].trim().to_string())
            }
            None => (Vec::new(), php.to_string()),
        };
        let code = sanitize_method(&code, m)?;
        let code = code.trim();
        match FUNC.captures(code) {
            Some(c) if &c[1] == m => {}
            _ => return Err(format!("method_php must define public function {m}(...)")),
        }
        let mut doc = format!("    /**\n     * @doctest {target}\n     * @dataProvider cases_{m}\n");
        for l in own_doc {
            doc += &format!("     * {l}\n");
        }
        doc += "     */\n";
        let mut section = format!(
            "\n{doc}    #[{DP}('cases_{m}')]\n{}\n\n{}",
            reindent(code, 4),
            provider(&format!("cases_{m}"), &render_rows(&normal, false))
        );
        if !bugs.is_empty() {
            section += &bug_section(target, m, &render_rows(&bugs, true));
        }
        let end_line = text[..block_end].rfind('\n').map(|i| i + 1).unwrap_or(block_end);
        text.insert_str(end_line, &section);
        if normal.is_empty() {
            // only bug rows: the method is a helper of its _known_bugs test, not a test with an empty data set
            text = set_test(&text, m, false);
        }
    }
    write_atomic(file, &text)
}

/// Replaces the signature and body of doctest method `method`, keeping its docblock, attributes and providers.
#[cfg(test)]
pub fn replace_method(file: &Path, method: &str, method_php: &str) -> Result<(), String> {
    let _lock = file_lock(file)?;
    replace_method_unlocked(file, method, method_php)
}

/// `replace_method` for a caller that already holds `file_lock(file)`.
pub fn replace_method_unlocked(file: &Path, method: &str, method_php: &str) -> Result<(), String> {
    let mut text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let code = method_php.trim();
    let code = match code
        .strip_prefix("/**")
        .and_then(|r| r.find("*/").map(|e| r[e + 2..].trim().to_string()))
    {
        Some(c) => c,
        None => code.to_string(),
    };
    let code = sanitize_method(&code, method)?.trim().to_string();
    match FUNC.captures(&code) {
        Some(c) if &c[1] == method => {}
        _ => return Err(format!("method_php must define public function {method}(...)")),
    }
    let (s, e) = function_span(&text, method).ok_or_else(|| format!("{method} not found in {}", file.display()))?;
    // keep the lines before the signature (docblock, attributes)
    let span = &text[s..e];
    let sig = Regex::new(&format!(
        r"(?m)^[ \t]*(?:public|protected|private)?\s*function\s+{}\s*\(",
        regex::escape(method)
    ))
    .unwrap();
    let at = sig.find(span).map(|m| m.start()).ok_or("signature not found")?;
    let was_test = span.contains(&format!("public function {method}("));
    let replacement = format!("{}{}\n", &span[..at], reindent(&code, 4));
    text.replace_range(s..e, &replacement);
    let main_rows = provider_rows(&text, &format!("cases_{method}")).len();
    if !was_test || main_rows == 0 {
        text = set_test(&text, method, main_rows > 0);
    }
    write_atomic(file, &text)
}

/// Inserts row text before the closing `];` of provider `name`.
fn add_rows(text: &str, name: &str, rows: &str) -> Result<String, String> {
    let (s, e) = function_span(text, name).ok_or_else(|| format!("provider {name} not found"))?;
    let close = text[s..e]
        .rfind("];")
        .ok_or_else(|| format!("provider {name} has no closing ];"))?
        + s;
    let line_start = text[..close].rfind('\n').map(|i| i + 1).unwrap_or(close);
    let mut out = text.to_string();
    out.insert_str(line_start, rows);
    Ok(out)
}

/// Removes rows from `method`'s providers; when none are left the method and its providers go too.
#[cfg(test)]
pub fn remove_rows(file: &Path, method: &str, cases: &[String]) -> Result<(), String> {
    let _lock = file_lock(file)?;
    remove_rows_unlocked(file, method, cases)
}

/// `remove_rows` for a caller that already holds `file_lock(file)`. Only touches doctest methods (those with a
/// `cases_<method>` provider inside the `// <doctest>` block).
pub fn remove_rows_unlocked(file: &Path, method: &str, cases: &[String]) -> Result<(), String> {
    let original = std::fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let in_block = |text: &str, name: &str| -> bool {
        match (text.find(START), text.find(END), function_span(text, name)) {
            (Some(s), Some(e), Some((fs, fe))) => fs >= s && fe <= e,
            _ => false,
        }
    };
    if !in_block(&original, &format!("cases_{method}")) {
        return Err(format!(
            "{method} is not a doctest method in {}; only rows doctest wrote can be removed",
            file.display()
        ));
    }
    let mut text = original;
    for provider_name in [format!("cases_{method}"), format!("bugs_{method}")] {
        text = drop_rows(&text, &provider_name, cases);
    }
    write_atomic(file, &settle_method(text, method))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunResult {
    pub passed: Vec<String>,
    /// (test name as printed, failure output, file:line)
    pub failed: Vec<(String, String, Option<String>)>,
    pub output: String,
}

fn norm_name(s: &str) -> String {
    let s = s.trim().to_lowercase().replace('_', " ");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.strip_prefix("test ").map(String::from).unwrap_or(s)
}

static DATASET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^(.*?)(?: with data set "(.*?)"|#(\S+))"#).unwrap());

fn cases_of(names: &[&str], method: &str) -> Vec<String> {
    let want = norm_name(method);
    let mut out = Vec::new();
    for n in names {
        if let Some(c) = DATASET.captures(n)
            && norm_name(&c[1]) == want
        {
            let case = c
                .get(2)
                .or(c.get(3))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !out.contains(&case) {
                out.push(case);
            }
        }
    }
    out
}

/// A test name as printed (humanized or not, with or without a data set) belongs to `method`.
pub fn same_method(printed: &str, method: &str) -> bool {
    let name = printed.rsplit("::").next().unwrap_or(printed);
    let base = DATASET
        .captures(name)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or(name);
    norm_name(base) == norm_name(method)
}

/// A data-set name as printed and a row's case key are the same case (Collision prints `partly_paid` as
/// `partly paid`).
pub fn same_case(printed: &str, case: &str) -> bool {
    let n = |s: &str| {
        s.to_lowercase()
            .replace('_', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    n(printed) == n(case)
}

impl RunResult {
    pub fn failed_cases(&self, method: &str) -> Vec<String> {
        cases_of(&self.failed.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), method)
    }

    pub fn passed_cases(&self, method: &str) -> Vec<String> {
        cases_of(&self.passed.iter().map(String::as_str).collect::<Vec<_>>(), method)
    }

    /// How row `case` of `method` did: Some(true) passed, Some(false) failed, None not run at all.
    pub fn row(&self, method: &str, case: &str) -> Option<bool> {
        if self.failed_cases(method).iter().any(|c| same_case(c, case)) {
            Some(false)
        } else if self.passed_cases(method).iter().any(|c| same_case(c, case)) {
            Some(true)
        } else {
            None
        }
    }

    /// Why row `case` of `method` failed: the file:line and the first lines of PHPUnit's message (the entry with
    /// text when the run printed the name more than once).
    pub fn failure(&self, method: &str, case: &str) -> (Option<String>, String) {
        let hits: Vec<&(String, String, Option<String>)> = self
            .failed
            .iter()
            .filter(|(name, _, _)| cases_of(&[name.as_str()], method).iter().any(|c| same_case(c, case)))
            .collect();
        let at = hits.iter().find_map(|h| h.2.clone());
        let text = hits
            .iter()
            .map(|h| h.1.as_str())
            .find(|t| !t.is_empty())
            .unwrap_or_default();
        (at, text.to_string())
    }
}

static DURATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\d+(?:\.\d+)?m?s$").unwrap());
static PHPUNIT_FAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+\) [^\s:]+::(.+)$").unwrap());
static AT_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|\s|at\s)(\S+\.php:\d+)\s*$").unwrap());

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\r").unwrap());

/// Output without colour and cursor codes.
pub fn strip_ansi(out: &str) -> String {
    ANSI.replace_all(out, "").into_owned()
}

/// Results from `php artisan test` (Collision) or plain PHPUnit output, coloured or not.
pub fn parse_output(out: &str) -> RunResult {
    let out = strip_ansi(out);
    let out = out.as_str();
    let mut r = RunResult {
        output: out.to_string(),
        ..RunResult::default()
    };
    let lines: Vec<&str> = out.lines().collect();
    for l in &lines {
        let t = l.trim();
        for (mark, passed) in [
            ("✓", true),
            ("✔", true),
            ("⨯", false),
            ("✘", false),
            ("×", false),
            ("✕", false),
            ("✗", false),
        ] {
            if let Some(rest) = t.strip_prefix(mark) {
                let name = DURATION.replace(rest.trim(), "").trim().to_string();
                if passed {
                    r.passed.push(name);
                } else {
                    r.failed.push((name, String::new(), None));
                }
            }
        }
    }
    // Collision failure sections
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if let Some(rest) = t.strip_prefix("FAILED") {
            let name = rest
                .rsplit_once(" > ")
                .map(|(_, n)| n.trim().to_string())
                .unwrap_or_default();
            let mut body = Vec::new();
            let mut at = None;
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("FAILED") && !lines[i].trim().starts_with("Tests:") {
                if at.is_none()
                    && let Some(c) = AT_LINE.captures(lines[i])
                {
                    at = Some(c[1].to_string());
                }
                body.push(lines[i]);
                i += 1;
            }
            let text = body.join("\n").trim().to_string();
            let key = |n: &str| {
                n.to_lowercase()
                    .replace('_', " ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let short = |n: &str| key(n).strip_prefix("test ").map(String::from).unwrap_or_else(|| key(n));
            match r.failed.iter_mut().find(|f| short(&f.0) == short(&name)) {
                Some(f) => {
                    f.1 = text;
                    f.2 = at;
                }
                None => r.failed.push((name, text, at)),
            }
            continue;
        }
        if let Some(c) = PHPUNIT_FAIL.captures(t) {
            let mut name = c[1].to_string();
            if let Some(idx) = name.find("\" (") {
                name.truncate(idx + 1);
            }
            let mut body = Vec::new();
            let mut at = None;
            i += 1;
            while i < lines.len()
                && !PHPUNIT_FAIL.is_match(lines[i].trim())
                && !lines[i].starts_with("FAILURES!")
                && !lines[i].starts_with("ERRORS!")
            {
                if let Some(cap) = AT_LINE.captures(lines[i]) {
                    at = Some(cap[1].to_string());
                }
                body.push(lines[i]);
                i += 1;
            }
            r.failed.push((name, body.join("\n").trim().to_string(), at));
            continue;
        }
        i += 1;
    }
    r
}

/// PHPUnit --filter regex matching these methods of one class (all their data sets).
pub fn filter_for(class: &str, methods: &[String]) -> String {
    format!("/{class}::(?:{})\\b/", methods.join("|"))
}

/// `value` quoted for where `{filter}` stands in the run line: inside a single-quoted `sh -c '…'` it is double
/// quoted for the inner shell, elsewhere single quoted.
pub fn quote_for(template: &str, value: &str) -> String {
    let at = template.find("{filter}").unwrap_or(0);
    let mut in_single = false;
    let mut in_double = false;
    let mut prev = ' ';
    for ch in template[..at].chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single && prev != '\\' => in_double = !in_double,
            _ => {}
        }
        prev = ch;
    }
    if in_single {
        let v = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`");
        if value.contains('\'') {
            // a single quote cannot stand inside the outer single quotes: close, add it escaped, reopen
            format!("\"{}\"", v.replace('\'', "'\\''"))
        } else {
            format!("\"{v}\"")
        }
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Runs `[tests] run` with `--filter <filter>` in the backend repo; with `file`, only that test file is loaded
/// (another worker's file being written cannot break the run).
pub fn run(cfg: &Config, filter: &str) -> Result<RunResult, String> {
    run_in(cfg, None, filter)
}

pub fn run_in(cfg: &Config, file: Option<&str>, filter: &str) -> Result<RunResult, String> {
    let t = &cfg.tests.run;
    let mut arg = String::new();
    if let Some(f) = file {
        arg += &quote_for(t, f);
        arg.push(' ');
    }
    arg += &format!("--filter {}", quote_for(t, filter));
    let cmd = t.replace("{filter}", &arg);
    let out = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&cfg.backend)
        .output()
        .map_err(|e| format!("cannot run `{cmd}`: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let r = parse_output(&text);
    if r.passed.is_empty() && r.failed.is_empty() {
        let text = strip_ansi(&text);
        if out.status.success() {
            return Err(format!("`{cmd}` ran no tests:\n{}", tail(&text, 40)));
        }
        return Err(format!("`{cmd}` failed:\n{}", tail(&text, 60)));
    }
    Ok(r)
}

/// Runs `[tests] run` on these test files (paths relative to the backend).
pub fn run_files(cfg: &Config, files: &[String]) -> Result<RunResult, String> {
    let args: Vec<String> = files.iter().map(|f| quote_for(&cfg.tests.run, f)).collect();
    let cmd = cfg.tests.run.replace("{filter}", &args.join(" "));
    let out = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&cfg.backend)
        .output()
        .map_err(|e| format!("cannot run `{cmd}`: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let r = parse_output(&text);
    if r.passed.is_empty() && r.failed.is_empty() && !out.status.success() {
        return Err(format!("`{cmd}` failed:\n{}", tail(&strip_ansi(&text), 60)));
    }
    Ok(r)
}

pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::tests::{fixtures, shop_config};
    use std::fs;

    const TARGET: &str = "POST api/orders/{order}";
    const BODY: &str = "public function test_store(bool $signIn, array $body, int $status): void\n{\n    $this->postJson('/api/orders/1', $body)->assertStatus($status);\n}";

    fn row(case: &str, args: &str) -> Row {
        Row {
            case: case.into(),
            args_php: args.into(),
            bug: None,
        }
    }

    fn store(rows: Vec<Row>) -> NewTests {
        NewTests {
            method: "test_store".into(),
            method_php: Some(BODY.into()),
            rows,
        }
    }

    #[test]
    fn target_file_feature_and_unit() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let (p, class, ns) = target_file(&cfg, TARGET, "App\\Http\\Controllers\\OrderController@store");
        assert_eq!(
            p,
            cfg.backend
                .join("tests/Feature/Http/Controllers/OrderControllerTest.php")
        );
        assert_eq!(
            (class.as_str(), ns.as_str()),
            ("OrderControllerTest", "Tests\\Feature\\Http\\Controllers")
        );
        let (p, class, ns) = target_file(
            &cfg,
            "App\\Services\\Billing\\Invoicer::issue",
            "App\\Services\\Billing\\Invoicer::issue",
        );
        assert_eq!(p, cfg.backend.join("tests/Unit/Services/Billing/InvoicerTest.php"));
        assert_eq!(
            (class.as_str(), ns.as_str()),
            ("InvoicerTest", "Tests\\Unit\\Services\\Billing")
        );
    }

    #[test]
    fn insert_creates_file_with_provider() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("tests/Unit/XTest.php");
        insert(
            &f,
            "XTest",
            "Tests\\Unit",
            TARGET,
            &store(vec![
                row("ok", "true, ['email' => 'a@b.c'], 200"),
                row("guest", "false, [], 401"),
            ]),
        )
        .unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(s.starts_with("<?php\n\nnamespace Tests\\Unit;\n\nuse Tests\\TestCase;\n\nclass XTest extends TestCase\n{\n    // <doctest>\n"), "{s}");
        for line in [
            "    /**\n     * @doctest POST api/orders/{order}\n     * @dataProvider cases_test_store\n     */\n",
            "    #[\\PHPUnit\\Framework\\Attributes\\DataProvider('cases_test_store')]\n    public function test_store(bool $signIn, array $body, int $status): void\n    {\n",
            "    public static function cases_test_store(): array\n    {\n        return [\n",
            "            'ok' => [true, ['email' => 'a@b.c'], 200],\n            'guest' => [false, [], 401],\n        ];\n    }\n",
        ] {
            assert!(s.contains(line), "missing:\n{line}\nin:\n{s}");
        }
        assert!(s.trim_end().ends_with("    // </doctest>\n}"));
    }

    #[test]
    fn insert_appends_rows_to_existing_method() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        insert(&f, "XTest", "Tests", TARGET, &store(vec![row("ok", "true, [], 200")])).unwrap();
        let more = NewTests {
            method: "test_store".into(),
            method_php: None,
            rows: vec![row("not_found", "true, [], 404")],
        };
        insert(&f, "XTest", "Tests", TARGET, &more).unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert_eq!(s.matches("public function test_store(").count(), 1);
        assert!(
            s.contains("            'ok' => [true, [], 200],\n            'not_found' => [true, [], 404],\n        ];"),
            "{s}"
        );
    }

    #[test]
    fn insert_refuses_existing_case_row() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        insert(&f, "XTest", "Tests", TARGET, &store(vec![row("ok", "true, [], 200")])).unwrap();
        let again = NewTests {
            method: "test_store".into(),
            method_php: None,
            rows: vec![row("ok", "true, [], 201")],
        };
        let err = insert(&f, "XTest", "Tests", TARGET, &again).unwrap_err();
        assert!(err.contains("'ok'"), "{err}");
    }

    #[test]
    fn insert_leaves_parity_block_untouched() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("OrderControllerTest.php");
        let original =
            fs::read_to_string(fixtures().join("shop/tests/Feature/Http/Controllers/OrderControllerTest.php")).unwrap();
        fs::write(&f, &original).unwrap();
        insert(
            &f,
            "OrderControllerTest",
            "Tests\\Feature\\Http\\Controllers",
            TARGET,
            &store(vec![row("forbidden", "true, [], 403")]),
        )
        .unwrap();
        let s = fs::read_to_string(&f).unwrap();
        let block =
            |x: &str| x[x.find("// <laravel-parity>").unwrap()..x.find("// </laravel-parity>").unwrap()].to_string();
        assert_eq!(block(&s), block(&original));
        assert!(s.find("// </laravel-parity>").unwrap() < s.find("// <doctest>").unwrap());
        assert!(s.trim_end().ends_with("    // </doctest>\n}"));
    }

    #[test]
    fn insert_refuses_name_used_outside_block() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("OrderControllerTest.php");
        fs::copy(
            fixtures().join("shop/tests/Feature/Http/Controllers/OrderControllerTest.php"),
            &f,
        )
        .unwrap();
        let t = NewTests {
            method: "test_unrelated".into(),
            method_php: Some("public function test_unrelated(int $s): void {}".into()),
            rows: vec![row("ok", "200")],
        };
        let err = insert(&f, "OrderControllerTest", "Tests", TARGET, &t).unwrap_err();
        assert!(err.contains("test_unrelated"), "{err}");
    }

    #[test]
    fn method_php_name_must_match() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        let t = NewTests {
            method: "test_other".into(),
            method_php: Some(BODY.into()),
            rows: vec![row("ok", "true, [], 200")],
        };
        assert!(
            insert(&f, "XTest", "Tests", TARGET, &t)
                .unwrap_err()
                .contains("test_other")
        );
    }

    #[test]
    fn bug_rows_go_to_bug_provider_and_group() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        let mut b = row("forbidden", "true, [], 403");
        b.bug = Some("any user can create orders".into());
        insert(
            &f,
            "XTest",
            "Tests",
            TARGET,
            &store(vec![row("ok", "true, [], 200"), b]),
        )
        .unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(s.contains("    public static function bugs_test_store(): array\n    {\n        return [\n            // @doctest-bug any user can create orders\n            'forbidden' => [true, [], 403],\n"), "{s}");
        assert!(s.contains("     * @group doctest-bug\n"));
        assert!(s.contains("    #[\\PHPUnit\\Framework\\Attributes\\Group('doctest-bug')]\n    public function test_store_known_bugs(...$args): void\n    {\n        $this->test_store(...$args);\n    }\n"), "{s}");
        assert!(
            !s[s.find("cases_test_store(): array").unwrap()..s.find("bugs_test_store(): array").unwrap()]
                .contains("'forbidden'")
        );
    }

    #[test]
    fn remove_rows_and_whole_method_when_empty() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        insert(
            &f,
            "XTest",
            "Tests",
            TARGET,
            &store(vec![row("ok", "true, [], 200"), row("guest", "false, [], 401")]),
        )
        .unwrap();
        remove_rows(&f, "test_store", &["guest".to_string()]).unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(!s.contains("'guest'") && s.contains("'ok'"));
        remove_rows(&f, "test_store", &["ok".to_string()]).unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(!s.contains("test_store") && !s.contains("@doctest POST"), "{s}");
        assert!(s.contains("// <doctest>\n    // </doctest>"));
    }

    const ARTISAN: &str = "\n   FAIL  Tests\\Feature\\Http\\Controllers\\OrderControllerTest\n  ✓ store with data set \"ok\"                              0.12s\n  ⨯ store with data set \"invalid:email:required\"          0.05s\n  ────────────────────────────────────────────\n   FAILED  Tests\\Feature\\Http\\Controllers\\OrderControllerTest > store with data set \"invalid:email:required\"\n  Expected response status code [422] but received 200.\n\n  at tests/Feature/Http/Controllers/OrderControllerTest.php:41\n\n  Tests:    1 failed, 1 passed (2 assertions)\n";

    #[test]
    fn parses_artisan_test_output() {
        let r = parse_output(ARTISAN);
        assert_eq!(r.passed.len(), 1);
        assert_eq!(r.failed.len(), 1);
        assert!(r.failed[0].1.contains("Expected response status code [422]"));
        assert_eq!(r.failed_cases("test_store"), vec!["invalid:email:required"]);
        assert_eq!(r.passed_cases("test_store"), vec!["ok"]);
        assert_eq!(
            r.failed[0].2.as_deref(),
            Some("tests/Feature/Http/Controllers/OrderControllerTest.php:41")
        );
    }

    #[test]
    fn parses_phpunit_output() {
        let out = "PHPUnit 9.6\n\n.F\n\nThere was 1 failure:\n\n1) Tests\\Feature\\OrderTest::test_store with data set \"guest\" (false, array(), 401)\nExpected status code 401 but received 200.\n\n/app/tests/Feature/OrderTest.php:20\n\nFAILURES!\nTests: 2, Assertions: 2, Failures: 1.\n";
        let r = parse_output(out);
        assert_eq!(r.failed_cases("test_store"), vec!["guest"]);
        assert!(r.failed[0].1.contains("Expected status code 401"));
        assert_eq!(r.failed[0].2.as_deref(), Some("/app/tests/Feature/OrderTest.php:20"));
    }

    #[test]
    fn filter_regex_for_methods() {
        assert_eq!(
            filter_for("OrderControllerTest", &["test_a".into(), "test_b".into()]),
            "/OrderControllerTest::(?:test_a|test_b)\\b/"
        );
    }

    #[test]
    fn comments_and_heredocs_do_not_confuse_the_span() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        let body = "public function test_store(int $s): void\n{\n    // guest can't post\n    # it's fine\n    /* don't } */\n    $x = <<<EOT\n    it's } here\n    EOT;\n    $this->assertTrue(true);\n}";
        insert(
            &f,
            "XTest",
            "Tests",
            TARGET,
            &NewTests {
                method: "test_store".into(),
                method_php: Some(body.into()),
                rows: vec![row("ok", "200")],
            },
        )
        .unwrap();
        insert(
            &f,
            "XTest",
            "Tests",
            "GET api/x",
            &NewTests {
                method: "test_other".into(),
                method_php: Some("public function test_other(int $s): void {}".into()),
                rows: vec![row("ok", "200")],
            },
        )
        .unwrap();
        remove_rows(&f, "test_store", &["ok".to_string()]).unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(!s.contains("test_store") && !s.contains("cases_test_store"), "{s}");
        assert!(
            s.contains("public function test_other") && s.contains("'ok' => [200]"),
            "{s}"
        );
        assert!(s.trim_end().ends_with("    // </doctest>\n}"), "{s}");
    }

    #[test]
    fn brackets_inside_row_strings_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        insert(
            &f,
            "XTest",
            "Tests",
            TARGET,
            &store(vec![
                row("ok", "true, ['q' => 'a['], 200"),
                row("guest", "false, [], 401"),
            ]),
        )
        .unwrap();
        remove_rows(&f, "test_store", &["ok".to_string()]).unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(s.contains("'guest' => [false, [], 401],\n        ];"), "{s}");
    }

    #[test]
    fn parallel_inserts_into_one_file_all_land() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        std::thread::scope(|s| {
            for i in 0..8 {
                let f = &f;
                s.spawn(move || {
                    let m = format!("test_m{i}");
                    insert(
                        f,
                        "XTest",
                        "Tests",
                        &format!("GET api/m{i}"),
                        &NewTests {
                            method: m.clone(),
                            method_php: Some(format!("public function {m}(int $s): void {{}}")),
                            rows: vec![row("ok", "200")],
                        },
                    )
                    .unwrap();
                });
            }
        });
        let s = fs::read_to_string(&f).unwrap();
        for i in 0..8 {
            assert!(s.contains(&format!("function test_m{i}(")), "test_m{i} lost:\n{s}");
        }
    }

    #[test]
    fn replace_method_keeps_docblock_and_provider() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("XTest.php");
        insert(&f, "XTest", "Tests", TARGET, &store(vec![row("ok", "true, [], 200")])).unwrap();
        replace_method(&f, "test_store", "public function test_store(bool $signIn, array $body, int $status): void\n{\n    $this->assertSame(1, 1);\n}").unwrap();
        let s = fs::read_to_string(&f).unwrap();
        assert!(
            s.contains("        $this->assertSame(1, 1);") && !s.contains("assertStatus"),
            "{s}"
        );
        assert!(
            s.contains("@doctest POST api/orders/{order}") && s.contains("'ok' => [true, [], 200]"),
            "{s}"
        );
    }

    #[test]
    fn public_methods_read_from_php() {
        let php = "<?php\nnamespace App\\Services;\n\nabstract class Billing\n{\n    public function __construct() {}\n    public function issue($a): int\n    {\n        // don't }\n        if ($a) { throw new \\RuntimeException('x'); }\n        return 1;\n    }\n    private function hidden() {}\n    public static function make(): static { return new static(); }\n    abstract public function total(): int;\n}\n";
        let (class, methods) = public_methods(php).unwrap();
        assert_eq!(class, "App\\Services\\Billing");
        let names: Vec<&str> = methods.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["issue", "make"]);
        assert!(methods[0].1.contains("throw new \\RuntimeException") && methods[0].1.trim_end().ends_with('}'));
    }

    #[test]
    fn remove_rows_never_deletes_a_hand_written_method() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("OrderControllerTest.php");
        fs::copy(
            fixtures().join("shop/tests/Feature/Http/Controllers/OrderControllerTest.php"),
            &f,
        )
        .unwrap();
        assert!(
            remove_rows(&f, "test_unrelated", &["ok".to_string()])
                .unwrap_err()
                .contains("not a doctest method")
        );
        assert!(
            fs::read_to_string(&f)
                .unwrap()
                .contains("public function test_unrelated")
        );
    }

    #[test]
    fn non_ascii_php_does_not_panic() {
        let m = php_code_mask("<?php\n$naïve = 2; // café\n$s = 'Café';\n");
        assert!(!m.is_empty());
        let _ = bracket_delta("Café'], 200],"); // a line from inside a string: only must not panic
        assert_eq!(bracket_delta("'é' => ['a'],"), 0);
    }

    #[test]
    fn reindent_handles_non_ascii_spaces() {
        assert_eq!(reindent("\u{a0}\u{a0}a\n\u{a0}\u{a0}  b", 4), "    a\n      b");
    }

    #[test]
    fn filter_quoting_fits_the_run_line() {
        let f = "/FooTest::(?:test_bar)\\b/";
        assert_eq!(
            quote_for("php artisan test {filter}", f),
            "'/FooTest::(?:test_bar)\\b/'"
        );
        // inside sh -c '...': double quotes for the inner shell, the backslash kept
        let q = quote_for("docker exec app sh -c 'cd /srv/api && php artisan test {filter}'", f);
        assert_eq!(q, "\"/FooTest::(?:test_bar)\\\\b/\"");
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("sh -c 'printf %s {q}'"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), f);
        // inside double quotes the single quotes reach the inner shell
        assert_eq!(quote_for("sh -c \"php artisan test {filter}\"", "a b"), "'a b'");
    }

    #[test]
    fn collision_output_from_a_real_run_is_read() {
        // php artisan test on irems, colours on: underscores in the data-set name are printed as spaces
        let out = "  \x1b[30;42;1m PASS \x1b[39;49;22m\x1b[39m Tests\\Feature\\VendorTest\x1b[39m\n  \x1b[32;1m✓\x1b[39;22m\x1b[90m \x1b[39m\x1b[90mshow doctest with data set \"ok\"\x1b[39m\x1b[90m      \x1b[39m \x1b[90m1.07s\x1b[39m  \n  \x1b[32;1m✓\x1b[39;22m\x1b[90m \x1b[39m\x1b[90mshow doctest with data set \"state:md5 password upgraded\"\x1b[39m\x1b[90m  \x1b[39m \x1b[90m0.71s\x1b[39m  \n\n  \x1b[90mTests:\x1b[39m    \x1b[32;1m2 passed\x1b[39;22m\x1b[90m (7 assertions)\x1b[39m\n";
        let r = parse_output(out);
        assert_eq!(r.row("test_show_doctest", "ok"), Some(true));
        assert_eq!(r.row("test_show_doctest", "state:md5_password_upgraded"), Some(true));
        assert_eq!(r.row("test_show_doctest_known_bugs", "ok"), None);
    }

    #[test]
    fn coloured_output_is_read() {
        let r = parse_output("  \x1b[32;1m✓\x1b[0m store with data set \"ok\"   \x1b[2m0.12s\x1b[22m\r\n");
        assert_eq!(r.passed, vec!["store with data set \"ok\""]);
        assert_eq!(r.row("test_store", "ok"), Some(true));
        assert_eq!(r.row("test_store", "guest"), None);
        assert!(same_case("returns:partly paid", "returns:partly_paid"));
    }

    #[test]
    fn bug_only_method_is_a_helper_and_rows_move_between_providers() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("OrderControllerTest.php");
        let mut bug = row("forbidden", "403");
        bug.bug = Some("lets anyone in".into());
        let t = NewTests {
            method: "test_store".into(),
            method_php: Some(BODY.into()),
            rows: vec![bug],
        };
        insert(&f, "OrderControllerTest", "Tests\\Feature", TARGET, &t).unwrap();
        let php = fs::read_to_string(&f).unwrap();
        assert!(php.contains("protected function test_store("), "{php}");
        assert!(
            !php.contains("#[\\PHPUnit\\Framework\\Attributes\\DataProvider('cases_test_store')]"),
            "{php}"
        );
        assert!(php.contains("public function test_store_known_bugs("), "{php}");
        // the row comes back without bug: it moves to the main provider, the method is a test again
        let t = NewTests {
            method: "test_store".into(),
            method_php: None,
            rows: vec![row("forbidden", "403")],
        };
        insert(&f, "OrderControllerTest", "Tests\\Feature", TARGET, &t).unwrap();
        let php = fs::read_to_string(&f).unwrap();
        assert!(php.contains("public function test_store("), "{php}");
        assert!(
            php.contains("#[\\PHPUnit\\Framework\\Attributes\\DataProvider('cases_test_store')]"),
            "{php}"
        );
        assert!(!php.contains("known_bugs") && !php.contains("@doctest-bug"), "{php}");
        assert_eq!(provider_rows(&php, "cases_test_store").len(), 1);
    }

    #[test]
    fn method_php_with_its_own_provider_method_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("OrderControllerTest.php");
        let t = NewTests {
            method: "test_store".into(),
            method_php: Some(format!("{BODY}\npublic static function mine(): array {{ return []; }}")),
            rows: vec![row("ok", "200")],
        };
        let e = insert(&f, "OrderControllerTest", "Tests\\Feature", TARGET, &t).unwrap_err();
        assert!(e.contains("cases_test_store") && e.contains("mine"), "{e}");
    }

    #[test]
    fn same_named_controllers_get_their_own_test_files() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = shop_config(d.path());
        cfg.backend = d.path().to_path_buf();
        for c in [
            "app/Http/Controllers/Ap/CreditNoteController.php",
            "app/Http/Controllers/Ar/Collection/CreditNoteController.php",
        ] {
            fs::create_dir_all(d.path().join(c).parent().unwrap()).unwrap();
            fs::write(d.path().join(c), "<?php").unwrap();
        }
        let ap = d
            .path()
            .join("tests/Feature/Http/Controllers/Ap/CreditNoteControllerTest.php");
        fs::create_dir_all(ap.parent().unwrap()).unwrap();
        fs::write(
            &ap,
            "<?php\nnamespace Tests\\Feature\\Http\\Controllers\\Ap;\nclass CreditNoteControllerTest {}\n",
        )
        .unwrap();
        let (f, _, ns) = target_file(
            &cfg,
            "",
            "App\\Http\\Controllers\\Ar\\Collection\\CreditNoteController@store",
        );
        assert!(
            f.ends_with("tests/Feature/Http/Controllers/Ar/Collection/CreditNoteControllerTest.php"),
            "{f:?}"
        );
        assert_eq!(ns, "Tests\\Feature\\Http\\Controllers\\Ar\\Collection");
        let (f, _, _) = target_file(&cfg, "", "App\\Http\\Controllers\\Ap\\CreditNoteController@store");
        assert_eq!(f, ap);
    }
}
