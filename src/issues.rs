//! Code problems to fix, as points (file, line, reason, fix) an AI can pick up: anything not optimised, from N+1
//! queries and missing indexes to synchronous mail, unpaginated lists and frontend loops of API calls; plus failing
//! tests. Each rule is a small pattern check; add rules here.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};

use crate::codemap::{BACKEND, CodemapApi};
use crate::config::Config;
use crate::testwrite::{RunResult, function_span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    FailingTest,
    NPlusOne,
    MissingIndex,
    FilterInPhp,
    CountForExists,
    GetForFirst,
    Unpaginated,
    SyncMail,
    EnvOutsideConfig,
    SleepInRequest,
    RequestInLoop,
    VForKey,
    VIfWithVFor,
    HeavyImport,
    ConsoleLog,
}

impl IssueKind {
    pub fn label(self) -> &'static str {
        match self {
            IssueKind::FailingTest => "failing test",
            IssueKind::NPlusOne => "N+1 query",
            IssueKind::MissingIndex => "missing index",
            IssueKind::FilterInPhp => "filtering in PHP",
            IssueKind::CountForExists => "count instead of exists",
            IssueKind::GetForFirst => "get instead of first",
            IssueKind::Unpaginated => "unpaginated list",
            IssueKind::SyncMail => "mail sent during the request",
            IssueKind::EnvOutsideConfig => "env() outside config",
            IssueKind::SleepInRequest => "sleep in request code",
            IssueKind::RequestInLoop => "API call in a loop",
            IssueKind::VForKey => "v-for without :key",
            IssueKind::VIfWithVFor => "v-if with v-for",
            IssueKind::HeavyImport => "whole-library import",
            IssueKind::ConsoleLog => "console.log left in",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub kind: IssueKind,
    /// Relative to its repo.
    pub file: String,
    pub line: usize,
    pub reason: String,
    pub fix: String,
}

impl Issue {
    /// One line for the AI to act on.
    pub fn point(&self) -> String {
        format!(
            "{}:{} — {}: {}. Fix: {}",
            self.file,
            self.line,
            self.kind.label(),
            self.reason,
            self.fix
        )
    }
}

fn line_of(text: &str, idx: usize) -> usize {
    text[..idx.min(text.len())].matches('\n').count() + 1
}

/// Byte range of the `{...}` block opening at or after `from`.
fn block_after(text: &str, from: usize) -> Option<(usize, usize)> {
    let open = from + text[from..].find('{')?;
    let mut depth = 0;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

const NOT_MODELS: [&str; 22] = [
    "Str",
    "Arr",
    "Carbon",
    "Rule",
    "Validator",
    "Log",
    "Cache",
    "Hash",
    "Auth",
    "Gate",
    "Storage",
    "Config",
    "Session",
    "Lang",
    "Route",
    "Schema",
    "Http",
    "Mail",
    "Event",
    "Queue",
    "Response",
    "Request",
];

static FOREACH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"foreach\s*\(\s*(.+?)\s+as\s+(?:\$\w+\s*=>\s*)?&?\$(\w+)\s*\)").unwrap());
static CLOSURE_LOOP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"->(?:each|map|filter|transform|reject|flatMap|mapWithKeys)\(\s*(?:static\s+)?function\s*\(\s*(?:[\w\\?]+\s+)?\$(\w+)").unwrap()
});
static MODEL_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z]\w*)::(where\w*|find\w*|query|first\w*|count|exists|select|pluck|sum|max|min|avg|with|all|latest|oldest|orderBy\w*)\(").unwrap()
});
static DB_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bDB::(table|select|selectOne|statement)\(").unwrap());
static USE_MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^use\s+(App\\Models\\[\w\\]+)(?:\s+as\s+(\w+))?;").unwrap());
static COLUMN_USE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"->(where|orWhere|whereIn|whereNotIn|whereNull|whereNotNull|whereBetween|whereDate|orderBy|orderByDesc|groupBy|firstWhere|latest|oldest)\(\s*'([\w.]+)'").unwrap()
});
static FILTER_IN_PHP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(::all\(\)|->get\(\))\s*->\s*(where|whereIn|filter|reject|count|sum|avg|max|min|firstWhere|sortBy|sortByDesc|take|unique|contains)\b").unwrap()
});
static COUNT_EXISTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"->count\(\)\s*(?:>\s*0|>=\s*1|!=\s*0|!==\s*0)").unwrap());
static GET_FIRST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"->get\(\)\s*(?:\[0\]|->first\(\))").unwrap());
static SYNC_MAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Mail::(?:to\([^;]*?\)\s*->\s*)?send\(|Notification::send\(|->notify\(").unwrap());
static ENV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\benv\(\s*'").unwrap());
static SLEEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:sleep|usleep)\(").unwrap());
static LIST_METHOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"public function (index|list\w*|search\w*|all\w*)\s*\(").unwrap());

pub struct IssueScanner<'a> {
    api: &'a mut dyn CodemapApi,
    cfg: &'a Config,
    models: HashMap<String, Option<(String, Vec<String>)>>,
    tables: HashMap<String, Option<Value>>,
    reported_fk: BTreeSet<String>,
}

impl<'a> IssueScanner<'a> {
    pub fn new(api: &'a mut dyn CodemapApi, cfg: &'a Config) -> IssueScanner<'a> {
        IssueScanner {
            api,
            cfg,
            models: HashMap::new(),
            tables: HashMap::new(),
            reported_fk: BTreeSet::new(),
        }
    }

    /// (table, relation names) of a model class.
    fn model(&mut self, fqn: &str) -> Option<(String, Vec<String>)> {
        if let Some(m) = self.models.get(fqn) {
            return m.clone();
        }
        let found = self
            .api
            .call(BACKEND, "get_symbol", json!({"name": fqn}))
            .ok()
            .and_then(|a| {
                let m = &a.data["model"];
                let table = m["table"].as_str()?.to_string();
                let rels = m["relations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_str()?.split(':').next().map(|s| s.trim().to_string()))
                    .collect();
                Some((table, rels))
            });
        self.models.insert(fqn.to_string(), found.clone());
        found
    }

    fn table(&mut self, table: &str) -> Option<Value> {
        if let Some(t) = self.tables.get(table) {
            return t.clone();
        }
        let t = self
            .api
            .call(BACKEND, "get_schema", json!({"table": table}))
            .ok()
            .map(|a| a.data)
            .filter(|d| d["found"] == true);
        self.tables.insert(table.to_string(), t.clone());
        t
    }

    /// Short class name → model FQN, from the file's `use` lines, else App\Models\<Name>.
    fn resolve(uses: &BTreeMap<String, String>, short: &str) -> String {
        uses.get(short)
            .cloned()
            .unwrap_or_else(|| format!("App\\Models\\{short}"))
    }

    pub fn scan_files(&mut self, files: &[PathBuf]) -> Result<Vec<Issue>, String> {
        self.scan_files_with(files, &crate::progress::Silent)
    }

    pub fn scan_files_with(
        &mut self,
        files: &[PathBuf],
        progress: &dyn crate::progress::Progress,
    ) -> Result<Vec<Issue>, String> {
        let mut out = Vec::new();
        for (n, rel) in files.iter().enumerate() {
            progress.step(n + 1, files.len(), "scanning code for problems");
            let Ok(text) = std::fs::read_to_string(self.cfg.backend.join(rel)) else {
                continue;
            };
            let file = rel.to_string_lossy().replace('\\', "/");
            self.scan_php(&file, &text, &mut out);
        }
        out.sort_by(|a, b| (&a.file, a.line, a.kind).cmp(&(&b.file, b.line, b.kind)));
        out.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.kind == b.kind);
        Ok(group_repeats(out))
    }

    fn scan_php(&mut self, file: &str, text: &str, out: &mut Vec<Issue>) {
        let mut add = |kind, idx: usize, reason: String, fix: String| {
            out.push(Issue {
                kind,
                file: file.to_string(),
                line: line_of(text, idx),
                reason,
                fix,
            });
        };
        let uses: BTreeMap<String, String> = USE_MODEL
            .captures_iter(text)
            .map(|c| {
                let fqn = c[1].to_string();
                let short = c
                    .get(2)
                    .map(|a| a.as_str().to_string())
                    .unwrap_or_else(|| fqn.rsplit('\\').next().unwrap_or_default().to_string());
                (short, fqn)
            })
            .collect();
        // relations of the models this file uses
        let mut relations: BTreeSet<String> = BTreeSet::new();
        let mut shorts: BTreeSet<String> = uses.keys().cloned().collect();
        for c in MODEL_CALL.captures_iter(text) {
            if !NOT_MODELS.contains(&&c[1]) {
                shorts.insert(c[1].to_string());
            }
        }
        for s in &shorts {
            if let Some((_, rels)) = self.model(&Self::resolve(&uses, s)) {
                relations.extend(rels);
            }
        }

        let is_job = text.contains("implements ShouldQueue") || file.contains("/Jobs/") || file.contains("/Console/");

        // loops: queries and lazy relations inside
        let mut loops: Vec<(String, String, usize, usize)> = Vec::new(); // (source expr, var, body start, body end)
        for c in FOREACH.captures_iter(text) {
            if let Some((s, e)) = block_after(text, c.get(0).map(|m| m.end()).unwrap_or(0)) {
                loops.push((c[1].to_string(), c[2].to_string(), s, e));
            }
        }
        for c in CLOSURE_LOOP.captures_iter(text) {
            let m = c.get(0).map(|m| m.start()).unwrap_or(0);
            let stmt_start = text[..m].rfind([';', '{', '}']).map(|i| i + 1).unwrap_or(0);
            if let Some((s, e)) = block_after(text, c.get(0).map(|m| m.end()).unwrap_or(0)) {
                loops.push((text[stmt_start..m].trim().to_string(), c[1].to_string(), s, e));
            }
        }
        for (source, var, s, e) in &loops {
            let body = &text[*s..*e];
            for c in MODEL_CALL.captures_iter(body) {
                if NOT_MODELS.contains(&&c[1]) {
                    continue;
                }
                let at = s + c.get(0).map(|m| m.start()).unwrap_or(0);
                add(
                    IssueKind::NPlusOne,
                    at,
                    format!("{}::{}() runs a query for every item of the loop over {source}", &c[1], &c[2]),
                    "load the rows once before the loop (one query with whereIn, or a relation with ->with()) and look them up by key inside it".into(),
                );
            }
            for c in DB_CALL.captures_iter(body) {
                let at = s + c.get(0).map(|m| m.start()).unwrap_or(0);
                add(
                    IssueKind::NPlusOne,
                    at,
                    format!("DB::{}() runs a query for every item of the loop over {source}", &c[1]),
                    "query once before the loop and index the result by key".into(),
                );
            }
            let rel_use = Regex::new(&format!(r"\${}->(\w+)\b(\s*\()?", regex::escape(var))).unwrap();
            let fn_start = text[..*s].rfind("function ").unwrap_or(0);
            let before = &text[fn_start..*s];
            let src_var = source
                .trim()
                .trim_start_matches('$')
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or_default()
                .to_string();
            let assign = Regex::new(&format!(r"\${}\s*=\s*([^;]+);", regex::escape(&src_var)))
                .ok()
                .and_then(|re| re.captures_iter(before).last().map(|c| c[1].to_string()))
                .unwrap_or_default();
            let mut seen = BTreeSet::new();
            for c in rel_use.captures_iter(body) {
                let rel = c[1].to_string();
                if c.get(2).is_some() || !relations.contains(&rel) || !seen.insert(rel.clone()) {
                    continue;
                }
                let loaded = |t: &str| {
                    t.contains(&format!("'{rel}'"))
                        || t.contains(&format!("'{rel}."))
                        || t.contains(&format!("'{rel}:"))
                        || t.contains(&format!("\"{rel}\""))
                };
                let eager = (source.contains("with(") && loaded(source))
                    || (assign.contains("with(") && loaded(&assign))
                    || ((before.contains("->load(") || before.contains("->loadMissing(")) && loaded(before));
                if eager {
                    continue;
                }
                let at = s + c.get(0).map(|m| m.start()).unwrap_or(0);
                add(
                    IssueKind::NPlusOne,
                    at,
                    format!("${var}->{rel} loads the relation {rel} once per item of the loop over {source}"),
                    format!("eager-load it with ->with('{rel}') on the query that fills {source}"),
                );
            }
        }

        // API resources reading relations without whenLoaded
        if text.contains("extends JsonResource") || text.contains("extends ResourceCollection") {
            let class = Regex::new(r"class\s+(\w+?)(?:Resource|Collection)\b")
                .unwrap()
                .captures(text)
                .map(|c| c[1].to_string());
            let mut rels = relations.clone();
            if let Some(c) = &class
                && let Some((_, r)) = self.model(&format!("App\\Models\\{c}"))
            {
                rels.extend(r);
            }
            let re = Regex::new(r"\$this->(\w+)\s*->").unwrap();
            for c in re.captures_iter(text) {
                if rels.contains(&c[1]) {
                    let at = c.get(0).map(|m| m.start()).unwrap_or(0);
                    add(
                        IssueKind::NPlusOne,
                        at,
                        format!(
                            "the resource reads $this->{} directly, so a collection of N items runs N queries",
                            &c[1]
                        ),
                        format!("use $this->whenLoaded('{}') and eager-load it in the controller", &c[1]),
                    );
                }
            }
        }

        for c in FILTER_IN_PHP.captures_iter(text) {
            let at = c.get(0).map(|m| m.start()).unwrap_or(0);
            add(
                IssueKind::FilterInPhp,
                at,
                format!("{} loads every row, then ->{}() works on them in PHP", &c[1], &c[2]),
                format!(
                    "do it in the query: replace {}->{}(…) with the query builder's {}(…) before ->get()",
                    &c[1], &c[2], &c[2]
                ),
            );
        }
        for m in COUNT_EXISTS.find_iter(text) {
            add(
                IssueKind::CountForExists,
                m.start(),
                "counts all matching rows only to see if there is one".into(),
                "use ->exists()".into(),
            );
        }
        for m in GET_FIRST.find_iter(text) {
            add(
                IssueKind::GetForFirst,
                m.start(),
                "loads every matching row to use the first one".into(),
                "use ->first() on the query".into(),
            );
        }
        if !is_job {
            for m in SYNC_MAIL.find_iter(text) {
                let stmt_end = text[m.start()..].find(';').map(|i| m.start() + i).unwrap_or(text.len());
                if queued_class(&self.cfg.backend, text, &text[m.start()..stmt_end]) {
                    continue;
                }
                add(
                    IssueKind::SyncMail,
                    m.start(),
                    "sends mail or a notification while the request waits".into(),
                    "queue it: ->queue() instead of ->send(), or make the mailable/notification implement ShouldQueue"
                        .into(),
                );
            }
            for m in SLEEP.find_iter(text) {
                add(
                    IssueKind::SleepInRequest,
                    m.start(),
                    "sleep() holds the request and a PHP worker".into(),
                    "move the delayed work to a queued job with ->delay()".into(),
                );
            }
        }
        if !file.starts_with("config/") {
            for m in ENV.find_iter(text) {
                add(
                    IssueKind::EnvOutsideConfig,
                    m.start(),
                    "env() returns null once `php artisan config:cache` runs, which production should use".into(),
                    "add the key to a config file and read it with config()".into(),
                );
            }
        }
        if file.contains("Http/Controllers") {
            for c in LIST_METHOD.captures_iter(text) {
                let name = c[1].to_string();
                let Some((s, e)) = function_span(text, &name) else {
                    continue;
                };
                let body = &text[s..e];
                if [
                    "paginate(",
                    "simplePaginate(",
                    "cursorPaginate(",
                    "->limit(",
                    "->take(",
                    "->first(",
                ]
                .iter()
                .any(|p| body.contains(p))
                {
                    continue;
                }
                if let Some(i) = body.find("->get()").or_else(|| body.find("::all()")) {
                    add(
                        IssueKind::Unpaginated,
                        s + i,
                        format!("{name}() returns every row; the list grows without limit"),
                        "use ->paginate() (and page in the frontend) or limit the rows".into(),
                    );
                }
            }
        }

        // columns filtered or sorted on without an index
        for c in COLUMN_USE.captures_iter(text) {
            let at = c.get(0).map(|m| m.start()).unwrap_or(0);
            let stmt_start = text[..at].rfind([';', '{', '}']).map(|i| i + 1).unwrap_or(0);
            let stmt = &text[stmt_start..at];
            let (mut table, mut column) = (None, c[2].to_string());
            if let Some((t, col)) = c[2].split_once('.') {
                table = Some(t.to_string());
                column = col.to_string();
            } else if let Some(t) = DB_TABLE.captures(stmt) {
                table = Some(t[1].to_string());
            } else if let Some(m) = MODEL_CALL
                .captures_iter(stmt)
                .filter(|m| !NOT_MODELS.contains(&&m[1]))
                .last()
            {
                table = self.model(&Self::resolve(&uses, &m[1])).map(|(t, _)| t);
            }
            let Some(table) = table else { continue };
            let Some(schema) = self.table(&table) else { continue };
            let col = schema["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|x| x["name"] == column.as_str())
                .cloned();
            if let Some(col) = col
                && col["keys"].as_array().is_none_or(|k| k.is_empty())
                && index_helps(&column, col["type"].as_str().unwrap_or_default())
            {
                add(
                    IssueKind::MissingIndex,
                    at,
                    format!("{table}.{column} is used in {}() but has no index", &c[1]),
                    format!("add an index in a migration: $table->index('{column}') on {table}"),
                );
            }
        }
        // unindexed *_id columns of the tables this file queries
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for s in &shorts {
            if let Some((t, _)) = self.model(&Self::resolve(&uses, s)) {
                touched.insert(t);
            }
        }
        for t in touched {
            let Some(schema) = self.table(&t) else { continue };
            let (mfile, mline) = schema["created_in"]
                .as_str()
                .and_then(|c| c.rsplit_once(':'))
                .map(|(f, l)| (f.to_string(), l.parse().unwrap_or(1)))
                .unwrap_or_else(|| ("database/migrations".into(), 1));
            for col in schema["columns"].as_array().into_iter().flatten() {
                let name = col["name"].as_str().unwrap_or_default();
                if name.ends_with("_id")
                    && col["keys"].as_array().is_none_or(|k| k.is_empty())
                    && self.reported_fk.insert(format!("{t}.{name}"))
                {
                    out.push(Issue {
                        kind: IssueKind::MissingIndex,
                        file: mfile.clone(),
                        line: mline,
                        reason: format!("{t}.{name} looks like a foreign key but has no index; joins and lookups by it scan the table"),
                        fix: format!("add $table->index('{name}') (or a foreign key) on {t} in a new migration"),
                    });
                }
            }
        }
    }
}

static NEW_CLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"new\s+\\?([A-Z][\w\\]*)").unwrap());
static USE_ANY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^use\s+([\w\\]+?)(?:\s+as\s+(\w+))?;").unwrap());

/// The mailable/notification created in `stmt` implements ShouldQueue (so sending it is already queued).
fn queued_class(backend: &Path, file_text: &str, stmt: &str) -> bool {
    let Some(c) = NEW_CLASS.captures(stmt) else {
        return false;
    };
    let name = c[1].to_string();
    let fqn = if name.contains('\\') {
        name
    } else {
        USE_ANY
            .captures_iter(file_text)
            .find(|u| {
                u.get(2).map(|a| a.as_str()) == Some(name.as_str()) || u[1].rsplit('\\').next() == Some(name.as_str())
            })
            .map(|u| u[1].to_string())
            .unwrap_or(name)
    };
    let Some(rel) = fqn.strip_prefix("App\\") else {
        return false;
    };
    std::fs::read_to_string(backend.join("app").join(format!("{}.php", rel.replace('\\', "/"))))
        .map(|t| t.contains("ShouldQueue"))
        .unwrap_or(false)
}

/// An index is worth suggesting: not a soft-delete stamp, a yes/no flag or an enum.
fn index_helps(column: &str, ty: &str) -> bool {
    let ty = ty.to_lowercase();
    !(column == "deleted_at"
        || column.starts_with("is_")
        || column.contains("_is_")
        || ty.starts_with("tinyint(1)")
        || ty.starts_with("bool")
        || ty.starts_with("enum"))
}

/// One missing-index point per table.column, listing the other lines that use it.
fn group_repeats(issues: Vec<Issue>) -> Vec<Issue> {
    let mut out: Vec<Issue> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for i in issues {
        if i.kind == IssueKind::MissingIndex
            && let Some(col) = i
                .reason
                .split(" is used in ")
                .next()
                .filter(|_| i.reason.contains(" is used in "))
        {
            let key = col.to_string();
            if let Some(&idx) = seen.get(&key) {
                let first = &mut out[idx];
                if first.file == i.file {
                    if first.reason.ends_with(')') && first.reason.contains(" (also line") {
                        first
                            .reason
                            .insert_str(first.reason.len() - 1, &format!(", {}", i.line));
                    } else {
                        first.reason += &format!(" (also line {})", i.line);
                    }
                }
                continue;
            }
            seen.insert(key, out.len());
        }
        out.push(i);
    }
    out
}

/// Every PHP file under app/ (relative to the backend).
pub fn app_files(cfg: &Config) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(&cfg.backend, &cfg.backend.join("app"), &["php"], &mut out);
    out.sort();
    out
}

/// Every .vue/.ts/.js file under a frontend's src/ (relative to the frontend).
pub fn frontend_files(app: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(
        app,
        &crate::config::Config::src_root(app),
        &["vue", "ts", "js"],
        &mut out,
    );
    out.sort();
    out
}

fn collect(root: &Path, dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if name != "node_modules" && name != "vendor" && !name.to_string_lossy().starts_with('.') {
                collect(root, &p, exts, out);
            }
        } else if p.extension().is_some_and(|x| exts.iter().any(|e| x == *e))
            && let Ok(rel) = p.strip_prefix(root)
        {
            out.push(rel.to_path_buf());
        }
    }
}

static DB_TABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"DB::table\(\s*'(\w+)'").unwrap());
static V_IF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\sv-if=").unwrap());
static TAG_WITH_VFOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[\w-]+[^>]*?\sv-for=[^>]*>").unwrap());
static HEAVY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*import\s+(?:\w+|\*\s+as\s+\w+)\s+from\s+['"](lodash|moment|rxjs|date-fns)['"]"#).unwrap()
});
static JS_LOOP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bfor\s*\(|\.forEach\(|\.map\(\s*async").unwrap());
static JS_REQUEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:api|axios|http|\$http|client)\.(?:get|post|put|patch|delete)\(|\bfetch\(").unwrap()
});
static CONSOLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bconsole\.(?:log|debug)\(").unwrap());

/// Frontend rules for files of one app (paths relative to it).
pub fn frontend_issues(app: &Path, files: &[PathBuf]) -> Vec<Issue> {
    let mut out = Vec::new();
    for rel in files {
        let Ok(text) = std::fs::read_to_string(app.join(rel)) else {
            continue;
        };
        let file = rel.to_string_lossy().replace('\\', "/");
        let mut add = |kind, idx: usize, reason: &str, fix: &str| {
            out.push(Issue {
                kind,
                file: file.clone(),
                line: line_of(&text, idx),
                reason: reason.into(),
                fix: fix.into(),
            });
        };
        for m in TAG_WITH_VFOR.find_iter(&text) {
            let tag = m.as_str();
            if !tag.contains(":key=") && !tag.contains("v-bind:key=") {
                add(
                    IssueKind::VForKey,
                    m.start(),
                    "v-for without :key makes Vue re-render the whole list on changes",
                    "add :key with a stable id, e.g. :key=\"item.id\"",
                );
            }
            if V_IF.is_match(tag) {
                add(
                    IssueKind::VIfWithVFor,
                    m.start(),
                    "v-if on the same element as v-for runs for every item (and its priority differs between Vue 2 and 3)",
                    "filter the list in a computed property, or move v-if to a wrapping <template>",
                );
            }
        }
        for m in HEAVY.captures_iter(&text) {
            let at = m.get(0).map(|x| x.start()).unwrap_or(0);
            let lib = &m[1];
            let fix = match lib {
                "lodash" => "import only what you use: import debounce from 'lodash-es/debounce'",
                "moment" => "use dayjs or date-fns functions; moment is large and not tree-shaken",
                _ => "import only the functions you use",
            };
            add(
                IssueKind::HeavyImport,
                at,
                &format!("imports the whole {lib} library into the bundle"),
                fix,
            );
        }
        for m in JS_LOOP.find_iter(&text) {
            let Some((s, e)) = block_after(&text, m.end()) else {
                continue;
            };
            let body = &text[s..e];
            if !body.contains("await") && !m.as_str().contains("async") {
                continue;
            }
            if let Some(r) = JS_REQUEST.find(body) {
                add(
                    IssueKind::RequestInLoop,
                    s + r.start(),
                    "sends one API request per item of a loop",
                    "add an endpoint that takes all ids at once, or at least run them in parallel with Promise.all",
                );
            }
        }
        for m in CONSOLE.find_iter(&text) {
            add(
                IssueKind::ConsoleLog,
                m.start(),
                "debug output left in the code",
                "remove it, or use a logger that is silent in production",
            );
        }
    }
    out.sort_by(|a, b| (&a.file, a.line, a.kind).cmp(&(&b.file, b.line, b.kind)));
    out.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.kind == b.kind);
    out
}

/// Failed tests of a run, pointed at the test file and line.
pub fn failing_tests(r: &RunResult) -> Vec<Issue> {
    r.failed
        .iter()
        .map(|(name, output, at)| {
            let (file, line) = at
                .as_deref()
                .and_then(|a| a.rsplit_once(':'))
                .map(|(f, l)| {
                    let f = f.find("tests/").map(|i| &f[i..]).unwrap_or(f);
                    (f.to_string(), l.parse().unwrap_or(0))
                })
                .unwrap_or_default();
            let first = output.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed");
            Issue {
                kind: IssueKind::FailingTest,
                file,
                line,
                reason: format!("{name}: {first}"),
                fix: "read the test and the code it calls; fix the code when the test expects the right behaviour, else fix the test".into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemap::FixtureCodemap;
    use crate::pages::tests::{fixtures, shop_config};

    fn scan() -> Vec<Issue> {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let mut api = FixtureCodemap::new(fixtures().join("codemap"));
        let mut s = IssueScanner::new(&mut api, &cfg);
        s.scan_files(&[PathBuf::from("app/Services/OrderReport.php")]).unwrap()
    }

    fn at(issues: &[Issue], kind: IssueKind, line: usize) -> Option<&Issue> {
        issues.iter().find(|i| i.kind == kind && i.line == line)
    }

    #[test]
    fn lazy_relation_in_loop_is_n_plus_one() {
        let i = scan();
        let x = at(&i, IssueKind::NPlusOne, 15).unwrap_or_else(|| panic!("{i:#?}"));
        assert_eq!(x.file, "app/Services/OrderReport.php");
        assert!(x.reason.contains("customer"), "{}", x.reason);
        assert!(x.fix.contains("with('customer')"), "{}", x.fix);
    }

    #[test]
    fn query_in_loop_is_n_plus_one() {
        let i = scan();
        assert!(at(&i, IssueKind::NPlusOne, 16).is_some(), "{i:#?}");
    }

    #[test]
    fn eager_loaded_loop_is_fine() {
        let i = scan();
        assert!(at(&i, IssueKind::NPlusOne, 21).is_none(), "{i:#?}");
    }

    #[test]
    fn unindexed_sort_column_is_reported() {
        let i = scan();
        let x = at(&i, IssueKind::MissingIndex, 19).unwrap_or_else(|| panic!("{i:#?}"));
        assert!(x.reason.contains("orders.placed_at"), "{}", x.reason);
    }

    #[test]
    fn indexed_columns_are_fine() {
        let i = scan();
        assert!(
            !i.iter()
                .any(|x| x.reason.contains("orders.status") || x.reason.contains("order_lines.order_id")),
            "{i:#?}"
        );
    }

    #[test]
    fn unindexed_foreign_key_reported_at_migration() {
        let i = scan();
        let x = i
            .iter()
            .find(|x| x.reason.contains("orders.customer_id"))
            .unwrap_or_else(|| panic!("{i:#?}"));
        assert_eq!(x.kind, IssueKind::MissingIndex);
        assert_eq!(
            (x.file.as_str(), x.line),
            ("database/migrations/2026_01_01_000000_create_orders_table.php", 10)
        );
    }

    fn scan_more() -> Vec<Issue> {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let mut api = FixtureCodemap::new(fixtures().join("codemap"));
        let mut s = IssueScanner::new(&mut api, &cfg);
        let mut out = s
            .scan_files(&[
                PathBuf::from("app/Services/Slow.php"),
                PathBuf::from("app/Http/Controllers/OrderListController.php"),
                PathBuf::from("app/Http/Resources/OrderResource.php"),
            ])
            .unwrap();
        out.extend(frontend_issues(
            &cfg.frontends[0],
            &[PathBuf::from("src/pages/SlowList.vue")],
        ));
        out
    }

    #[test]
    fn backend_optimisation_rules() {
        let i = scan_more();
        for (kind, file, line) in [
            (IssueKind::FilterInPhp, "app/Services/Slow.php", 12),
            (IssueKind::CountForExists, "app/Services/Slow.php", 13),
            (IssueKind::GetForFirst, "app/Services/Slow.php", 14),
            (IssueKind::SyncMail, "app/Services/Slow.php", 16),
            (IssueKind::EnvOutsideConfig, "app/Services/Slow.php", 17),
            (IssueKind::SleepInRequest, "app/Services/Slow.php", 18),
            (
                IssueKind::Unpaginated,
                "app/Http/Controllers/OrderListController.php",
                11,
            ),
            (IssueKind::NPlusOne, "app/Http/Resources/OrderResource.php", 13),
        ] {
            assert!(
                i.iter().any(|x| x.kind == kind && x.file == file && x.line == line),
                "{kind:?} {file}:{line} not in {i:#?}"
            );
        }
        assert!(
            !i.iter().any(|x| x.file.ends_with("OrderResource.php") && x.line == 14),
            "whenLoaded is fine"
        );
        assert!(
            !i.iter().any(|x| x.kind == IssueKind::SyncMail && x.line == 19),
            "a ShouldQueue notification is already queued"
        );
    }

    #[test]
    fn frontend_optimisation_rules() {
        let i = scan_more();
        let f = "src/pages/SlowList.vue";
        for (kind, line) in [
            (IssueKind::VIfWithVFor, 3),
            (IssueKind::VForKey, 3),
            (IssueKind::HeavyImport, 8),
            (IssueKind::RequestInLoop, 12),
            (IssueKind::ConsoleLog, 14),
        ] {
            assert!(
                i.iter().any(|x| x.kind == kind && x.file == f && x.line == line),
                "{kind:?} {f}:{line} not in {i:#?}"
            );
        }
        assert!(!i.iter().any(|x| x.kind == IssueKind::VForKey && x.line == 4));
    }

    #[test]
    fn failing_tests_become_issues() {
        let r = crate::testwrite::parse_output(
            "   FAILED  Tests\\Feature\\OrderTest > store with data set \"guest\"\n  Expected response status code [401] but received 200.\n\n  at /opt/localhost/api/tests/Feature/OrderTest.php:41\n\n  Tests:  1 failed\n",
        );
        let i = failing_tests(&r);
        assert_eq!(i.len(), 1);
        assert_eq!((i[0].file.as_str(), i[0].line), ("tests/Feature/OrderTest.php", 41));
        assert!(i[0].reason.contains("Expected response status code [401]"));
    }

    #[test]
    fn repeats_grouped_and_flags_skipped() {
        let mk = |line| Issue {
            kind: IssueKind::MissingIndex,
            file: "a.php".into(),
            line,
            reason: "t.c is used in where() but has no index".into(),
            fix: "f".into(),
        };
        let g = group_repeats(vec![mk(3), mk(9), mk(12)]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].reason, "t.c is used in where() but has no index (also line 9, 12)");
        assert!(!index_helps("deleted_at", "bigint"));
        assert!(!index_helps("leaserenewal_is_updated", "int"));
        assert!(!index_helps("active", "tinyint(1)"));
        assert!(index_helps("leaserenewal_project_id", "bigint"));
    }

    #[test]
    fn point_format() {
        let x = Issue {
            kind: IssueKind::NPlusOne,
            file: "app/A.php".into(),
            line: 3,
            reason: "r".into(),
            fix: "f".into(),
        };
        assert_eq!(x.point(), "app/A.php:3 — N+1 query: r. Fix: f");
    }
}
