//! Pages of the frontends and the facts behind each: labels and literal text on screen, the endpoints it calls with
//! their permissions, validation rules and messages, its menu entry. The fingerprint of these facts tells when a
//! page's guide and screenshots are out of date.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::codemap::{BACKEND, CodemapApi};
use crate::config::Config;
use crate::i18n::{Labels, Node, parse_tree};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// Route name, else path.
    pub key: String,
    pub app: String,
    pub path: String,
    pub route_name: Option<String>,
    /// Relative to the app.
    pub component: PathBuf,
    pub module: String,
    /// The guide (audience) it belongs to; "" without portals.
    #[serde(default)]
    pub portal: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub method: String,
    pub uri: String,
    pub route_name: Option<String>,
    pub action: String,
    /// `file:line` of the action in the backend.
    pub at: Option<String>,
    pub middleware: Vec<String>,
    pub permissions: Vec<String>,
    pub rules: BTreeMap<String, Vec<String>>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageFacts {
    pub page: Page,
    /// Language → screen text from i18n keys.
    pub labels: BTreeMap<String, Vec<String>>,
    /// Text written straight into the templates.
    pub literals: Vec<String>,
    pub components: Vec<String>,
    /// Every frontend file the page's code comes from (components, composables, imported modules).
    #[serde(default)]
    pub code_files: Vec<String>,
    pub endpoints: Vec<Endpoint>,
    /// Menu group and entry label of the page.
    pub menu_titles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuEntry {
    pub route_name: String,
    pub label: String,
    pub module: String,
    pub group: String,
    /// Permission code of the menu entry (e.g. `aro`), when the menu file has one.
    pub aro: String,
}

/// Builds pages and facts, caching backend lookups for one run.
pub struct FactBuilder<'a> {
    api: &'a mut dyn CodemapApi,
    cfg: &'a Config,
    labels: &'a Labels,
    pub menu: Vec<MenuEntry>,
    endpoints: HashMap<(String, String), Option<Endpoint>>,
    /// app → file → its API calls (one paged api_usage per app).
    calls: HashMap<String, HashMap<String, Vec<Value>>>,
    /// (app, component name) → get_component answer, shared across pages.
    components: HashMap<(String, String), Value>,
    /// (method, uri) → route, from one paged list_routes.
    routes: Option<HashMap<(String, String), Value>>,
    /// Any codemap answer was stale.
    pub stale: bool,
}

impl<'a> FactBuilder<'a> {
    pub fn new(api: &'a mut dyn CodemapApi, cfg: &'a Config, labels: &'a Labels) -> FactBuilder<'a> {
        let menu = cfg
            .guide
            .modules_from
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| parse_menu(&t))
            .unwrap_or_default();
        FactBuilder {
            api,
            cfg,
            labels,
            menu,
            endpoints: HashMap::new(),
            calls: HashMap::new(),
            components: HashMap::new(),
            routes: None,
            stale: false,
        }
    }

    fn app_dir(&self, app: &str) -> PathBuf {
        if app == BACKEND {
            return self.cfg.backend.clone();
        }
        self.cfg
            .frontends
            .iter()
            .find(|f| Config::repo_name(f) == app)
            .cloned()
            .unwrap_or_default()
    }

    fn call(&mut self, app: &str, tool: &str, args: Value) -> Result<Value, String> {
        let a = self.api.call(app, tool, args)?;
        self.stale |= a.stale;
        Ok(a.data)
    }

    pub fn pages(&mut self) -> Result<Vec<Page>, String> {
        let mut out: Vec<Page> = Vec::new();
        for fe in self.cfg.frontends.clone() {
            let app = Config::repo_name(&fe);
            let (items, stale) = self.api.paged(&app, "list_pages", json!({"detail": "full"}), "pages")?;
            self.stale |= stale;
            for p in items {
                let Some(file) = p["file"].as_str() else { continue };
                if p["redirect"].as_str().is_some_and(|r| !r.is_empty()) {
                    continue;
                }
                if !fe.join(file).is_file() {
                    continue;
                }
                let path = p["path"].as_str().unwrap_or_default().to_string();
                let route_name = p["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
                let key = route_name.clone().unwrap_or_else(|| path.clone());
                if out.iter().any(|o| o.key == key) {
                    continue;
                }
                let g = &self.cfg.guide;
                if (!g.include.is_empty() && !crate::work::page_matches(&key, &path, &g.include))
                    || crate::work::page_matches(&key, &path, &g.exclude)
                {
                    continue;
                }
                let module = module_for(&self.menu, route_name.as_deref().unwrap_or_default(), &path)
                    .unwrap_or_else(|| module_from_path(&path));
                let portal = self.cfg.portal_of(&key, &path);
                out.push(Page {
                    key,
                    app: app.clone(),
                    path,
                    route_name,
                    component: PathBuf::from(file),
                    module,
                    portal,
                });
            }
        }
        out.sort_by(|a, b| (&a.module, &a.key).cmp(&(&b.module, &b.key)));
        Ok(out)
    }

    pub fn facts(&mut self, page: &Page) -> Result<PageFacts, String> {
        let dir = self.app_dir(&page.app);
        let mut labels: BTreeMap<String, BTreeSet<String>> = self
            .cfg
            .languages
            .iter()
            .map(|l| (l.clone(), BTreeSet::new()))
            .collect();
        let mut literals = BTreeSet::new();
        let mut components = Vec::new();
        let mut code_files: BTreeSet<String> = BTreeSet::new();
        let mut composable_names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut queue: Vec<(String, usize)> = vec![(page.component.to_string_lossy().into_owned(), 0)];
        while let Some((file, depth)) = queue.pop() {
            if components.contains(&file) {
                continue;
            }
            components.push(file.clone());
            code_files.insert(file.clone());
            if let Ok(text) = std::fs::read_to_string(dir.join(&file)) {
                literals.extend(template_literals(&text));
            }
            let name = Path::new(&file)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let ck = (page.app.clone(), name.clone());
            let c = match self.components.get(&ck) {
                Some(c) => c.clone(),
                None => {
                    let c = self.call(&page.app, "get_component", json!({"name": name}))?;
                    self.components.insert(ck, c.clone());
                    c
                }
            };
            if c["found"] != true {
                continue;
            }
            for key in c["i18n"]["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                for (lang, set) in labels.iter_mut() {
                    if let Some(t) = self.labels.text(lang, key) {
                        set.insert(t.trim().to_string());
                    }
                }
            }
            for r in c["renders"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if let Some(target) = arrow_target(r)
                    && depth < 3
                    && dir.join(&target).is_file()
                {
                    queue.push((target, depth + 1));
                }
            }
            for r in c["composables"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if let Some(target) = arrow_target(r) {
                    let name = r.split_whitespace().next().unwrap_or_default().to_string();
                    composable_names.entry(target.clone()).or_default().insert(name);
                    code_files.insert(target);
                }
            }
        }
        // the API modules the page's code imports directly; for a module only the functions imported by name count
        let mut imported: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
        for f in code_files.clone() {
            let Ok(text) = std::fs::read_to_string(dir.join(&f)) else {
                continue;
            };
            for (imp, names) in local_imports(&dir, &f, &text) {
                if code_files.contains(&imp) {
                    continue;
                }
                match (imported.get_mut(&imp), names) {
                    (Some(Some(have)), Some(n)) => have.extend(n),
                    (Some(slot), None) => *slot = None,
                    (Some(None), Some(_)) => {}
                    (None, n) => {
                        imported.insert(imp, n.map(|v| v.into_iter().collect()));
                    }
                }
            }
        }
        let mut seen = BTreeSet::new();
        let mut endpoints = Vec::new();
        let scan_files_list: Vec<String> = imported.keys().cloned().collect();
        let scan: Vec<(String, Option<BTreeSet<String>>)> = code_files
            .iter()
            .filter(|f| !f.ends_with(".vue") || components.contains(f))
            .map(|f| (f.clone(), composable_names.get(f).cloned()))
            .chain(imported)
            .collect();
        self.load_calls(&page.app)?;
        for (f, names) in scan {
            let calls = self
                .calls
                .get(&page.app)
                .and_then(|m| m.get(&f))
                .cloned()
                .unwrap_or_default();
            let module_text = names.as_ref().and_then(|_| std::fs::read_to_string(dir.join(&f)).ok());
            for call in calls {
                let Some(at) = call["at"].as_str() else { continue };
                if call["status"] != "matched" || !at.starts_with(&format!("{f}:")) {
                    continue;
                }
                if let (Some(names), Some(text)) = (&names, &module_text) {
                    let line: usize = at.rsplit(':').next().and_then(|l| l.parse().ok()).unwrap_or(0);
                    match enclosing_export(text, line) {
                        Some(name) if !names.contains(&name) => continue,
                        _ => {}
                    }
                }
                let Some((methods, uri)) = call["route"].as_str().and_then(|r| r.split_once(' ')) else {
                    continue;
                };
                let method = methods.split('|').next().unwrap_or_default().to_string();
                if seen.insert((method.clone(), uri.to_string()))
                    && let Some(e) = self.endpoint(&method, uri)?
                {
                    endpoints.push(e);
                }
            }
        }
        // a page in several menus belongs to the one whose permission its endpoints check
        let mut page = page.clone();
        if let Some(r) = page.route_name.as_deref() {
            let perms: Vec<&String> = endpoints.iter().flat_map(|e| e.permissions.iter()).collect();
            if let Some(m) = self
                .menu
                .iter()
                .find(|m| m.route_name == r && !m.aro.is_empty() && perms.contains(&&m.aro))
            {
                page.module = m.module.clone();
            }
        }
        let page = &page;
        let menu_titles = page
            .route_name
            .as_deref()
            .map(|r| {
                let mut t: Vec<String> = Vec::new();
                for m in self.menu.iter().filter(|m| m.route_name == r) {
                    for s in [&m.group, &m.label] {
                        if !s.is_empty() && !t.contains(s) {
                            t.push(s.clone());
                        }
                    }
                }
                t
            })
            .unwrap_or_default();
        Ok(PageFacts {
            page: page.clone(),
            labels: labels.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
            literals: literals.into_iter().collect(),
            components,
            code_files: code_files
                .iter()
                .cloned()
                .chain(scan_files_list)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            endpoints,
            menu_titles,
        })
    }

    fn load_calls(&mut self, app: &str) -> Result<(), String> {
        if self.calls.contains_key(app) {
            return Ok(());
        }
        let (calls, stale) = self
            .api
            .paged(app, "api_usage", json!({"file": "src/", "detail": "full"}), "calls")?;
        self.stale |= stale;
        let mut by_file: HashMap<String, Vec<Value>> = HashMap::new();
        for c in calls {
            if let Some((file, _)) = c["at"].as_str().and_then(|a| a.rsplit_once(':')) {
                by_file.entry(file.to_string()).or_default().push(c);
            }
        }
        self.calls.insert(app.to_string(), by_file);
        Ok(())
    }

    /// Every backend route (one paged list_routes), keyed by (method, uri).
    pub fn all_routes(&mut self) -> Result<&HashMap<(String, String), Value>, String> {
        if self.routes.is_none() {
            let (routes, stale) = self.api.paged(
                BACKEND,
                "list_routes",
                json!({"detail": "full", "permissions": true}),
                "routes",
            )?;
            self.stale |= stale;
            let mut map = HashMap::new();
            for r in routes {
                let uri = r["uri"].as_str().unwrap_or_default().to_string();
                for m in r["methods"].as_str().unwrap_or_default().split('|') {
                    map.entry((m.to_string(), uri.clone())).or_insert_with(|| r.clone());
                }
            }
            self.routes = Some(map);
        }
        Ok(self.routes.as_ref().expect("loaded"))
    }

    /// The backend route `METHOD uri` with its permissions, rules and messages (cached).
    pub fn endpoint(&mut self, method: &str, uri: &str) -> Result<Option<Endpoint>, String> {
        let key = (method.to_string(), uri.to_string());
        if let Some(e) = self.endpoints.get(&key) {
            return Ok(e.clone());
        }
        let route = self.all_routes()?.get(&key).cloned();
        let Some(route) = route else {
            self.endpoints.insert(key, None);
            return Ok(None);
        };
        let mut e = endpoint_from_route(&route, method);
        let action = e.action.clone();
        if let Some((class, m)) = action.split_once('@') {
            let sym = self.call(
                BACKEND,
                "get_symbol",
                json!({"name": format!("{class}::{m}"), "detail": "full"}),
            )?;
            if let Some(src) = self.source_of(&sym) {
                e.at = Some(format!("{}:{}", sym["file"].as_str().unwrap_or_default(), sym["line"]));
                e.rules = parse_rules(&src);
                e.messages = parse_messages(&src);
            }
            let requests: Vec<String> = sym["meta"]["params"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|p| p["type_fqns"].as_array().cloned().unwrap_or_default())
                .filter_map(|t| t.as_str().map(String::from))
                .filter(|t| t.starts_with("App\\Http\\Requests"))
                .collect();
            for req in requests {
                let r = self.call(
                    BACKEND,
                    "get_symbol",
                    json!({"name": format!("{req}::rules"), "detail": "full"}),
                )?;
                if let Some(src) = self.source_of(&r) {
                    for (k, v) in parse_rules(&src) {
                        e.rules.entry(k).or_insert(v);
                    }
                }
                let m = self.call(
                    BACKEND,
                    "get_symbol",
                    json!({"name": format!("{req}::messages"), "detail": "full"}),
                )?;
                if let Some(src) = self.source_of(&m) {
                    e.messages.extend(quoted_values(&src));
                }
            }
        }
        self.endpoints.insert(key, Some(e.clone()));
        Ok(Some(e))
    }

    pub fn api(&mut self) -> &mut dyn CodemapApi {
        &mut *self.api
    }

    /// A symbol (detail full) and its source lines.
    pub fn symbol_source(&mut self, fqn: &str) -> Result<Option<(Value, String)>, String> {
        let sym = self.call(BACKEND, "get_symbol", json!({"name": fqn, "detail": "full"}))?;
        Ok(self.source_of(&sym).map(|s| (sym, s)))
    }

    pub fn source_of(&self, sym: &Value) -> Option<String> {
        if sym["found"] != true {
            return None;
        }
        let file = sym["file"].as_str()?;
        let line = sym["line"].as_u64()? as usize;
        let end = sym["end_line"].as_u64().map(|e| e as usize).unwrap_or(line + 80);
        let text = std::fs::read_to_string(self.cfg.backend.join(file)).ok()?;
        Some(
            text.lines()
                .skip(line.saturating_sub(1))
                .take(end + 1 - line)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// Something the user does on a page, from one of its endpoints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub name: String,
    pub endpoint: String,
    pub fields: Vec<String>,
    pub required: Vec<String>,
    /// Messages that read as success.
    pub success: Vec<String>,
    pub errors: Vec<String>,
}

const NAMED_ACTIONS: [&str; 22] = [
    "approve",
    "reject",
    "submit",
    "import",
    "export",
    "upload",
    "generate",
    "cancel",
    "void",
    "post",
    "send",
    "restore",
    "duplicate",
    "copy",
    "print",
    "download",
    "pdf",
    "xls",
    "excel",
    "terminate",
    "renew",
    "publish",
];

fn action_name(e: &Endpoint) -> Option<String> {
    let segs: Vec<&str> = e.uri.split('/').filter(|s| !s.is_empty()).collect();
    let last_literal = segs
        .iter()
        .rev()
        .find(|s| !s.starts_with('{'))
        .copied()
        .unwrap_or_default()
        .to_lowercase();
    let named = NAMED_ACTIONS.iter().find(|n| {
        last_literal == **n || last_literal.starts_with(&format!("{n}-")) || last_literal.ends_with(&format!("-{n}"))
    });
    let title = |s: &str| {
        let s = match s {
            "pdf" | "xls" | "excel" => "export",
            other => other,
        };
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
            .unwrap_or_default()
    };
    match (e.method.as_str(), named) {
        (_, Some(n)) => Some(title(n)),
        ("GET" | "HEAD", None) => None,
        ("DELETE", None) => Some("Delete".into()),
        ("PUT" | "PATCH", None) => Some("Edit".into()),
        ("POST", None) if segs.last().is_some_and(|s| s.starts_with('{')) => Some("Edit".into()),
        ("POST", None) => Some("Add".into()),
        _ => None,
    }
}

fn reads_as_success(m: &str) -> bool {
    let l = m.to_lowercase();
    [
        "success",
        "saved",
        "created",
        "updated",
        "deleted",
        "removed",
        "approved",
        "submitted",
        "imported",
        "sent",
        "added",
    ]
    .iter()
    .any(|w| l.contains(w))
        && !["fail", "cannot", "can't", "unable", "error", "not "]
            .iter()
            .any(|w| l.contains(w))
}

/// The page's actions, one per name, in the order of its endpoints.
pub fn actions(f: &PageFacts) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    for e in &f.endpoints {
        let Some(name) = action_name(e) else { continue };
        if out.iter().any(|a| a.name == name) {
            continue;
        }
        let (success, errors): (Vec<String>, Vec<String>) =
            e.messages.iter().cloned().partition(|m| reads_as_success(m));
        out.push(Action {
            name,
            endpoint: format!("{} {}", e.method, e.uri),
            fields: e.rules.keys().cloned().collect(),
            required: e
                .rules
                .iter()
                .filter(|(_, r)| r.iter().any(|x| x == "required"))
                .map(|(k, _)| k.clone())
                .collect(),
            success,
            errors,
        });
    }
    out
}

/// The screenshots a page guide should have.
pub fn screenshot_plan(actions: &[Action]) -> Vec<String> {
    let mut out = vec!["the page as it opens (list or form, with demo data)".to_string()];
    for a in actions {
        match a.name.as_str() {
            "Add" => {
                out.push("Add: the empty form".into());
                out.push("Add: the form filled in, before saving".into());
                out.push("Add: the result after saving (the message and the new row)".into());
                if !a.required.is_empty() {
                    out.push(format!(
                        "Add: a validation error (save with {} empty)",
                        a.required.join(", ")
                    ));
                }
            }
            "Edit" => {
                out.push("Edit: the form with a record loaded".into());
                out.push("Edit: the result after saving".into());
            }
            "Delete" => {
                out.push("Delete: the confirm dialog".into());
                out.push("Delete: the list after deleting".into());
            }
            other => {
                out.push(format!("{other}: before (the button or dialog)"));
                out.push(format!("{other}: the result"));
            }
        }
    }
    out
}

/// An endpoint from a `list_routes` (detail full) entry, without rules and messages.
pub fn endpoint_from_route(route: &Value, method: &str) -> Endpoint {
    let mut middleware: Vec<String> = Vec::new();
    for m in route["middleware"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !middleware.iter().any(|x| x == m) {
            middleware.push(m.to_string());
        }
    }
    let mut permissions: Vec<String> = Vec::new();
    for m in &middleware {
        for prefix in ["check.permission:", "permission:", "can:"] {
            if let Some(rest) = m.strip_prefix(prefix) {
                for p in rest.split(['|', ',']) {
                    if !p.is_empty() && !permissions.iter().any(|x| x == p) {
                        permissions.push(p.to_string());
                    }
                }
            }
        }
    }
    let method = if method.is_empty() {
        route["methods"]
            .as_str()
            .unwrap_or_default()
            .split('|')
            .next()
            .unwrap_or_default()
    } else {
        method
    };
    Endpoint {
        method: method.to_string(),
        uri: route["uri"].as_str().unwrap_or_default().to_string(),
        route_name: route["name"].as_str().map(String::from),
        action: route["action"].as_str().unwrap_or_default().to_string(),
        middleware,
        permissions,
        ..Endpoint::default()
    }
}

/// The menu module of a page: of the menu entries for its route, the one whose module name is a segment of the
/// path (a page listed under two menus belongs where its URL says), else the first.
pub fn module_for(menu: &[MenuEntry], route: &str, path: &str) -> Option<String> {
    let entries: Vec<&MenuEntry> = menu
        .iter()
        .filter(|m| !route.is_empty() && m.route_name == route)
        .collect();
    let segs: Vec<String> = path.split('/').map(|s| s.to_lowercase()).collect();
    entries
        .iter()
        .find(|m| segs.contains(&m.module.to_lowercase()))
        .or(entries.first())
        .map(|m| m.module.clone())
}

fn module_from_path(path: &str) -> String {
    path.split('/')
        .find(|s| !s.is_empty() && *s != "admin" && !s.starts_with(':'))
        .unwrap_or("general")
        .to_string()
}

/// `<Foo> :4 → src/components/Foo.vue` or `useX :12 → src/x.ts:2` → the file, unless external.
fn arrow_target(s: &str) -> Option<String> {
    let (_, target) = s.split_once('→')?;
    let target = target.trim();
    if target.starts_with("external") {
        return None;
    }
    let file = target.split_whitespace().next()?;
    let file = match file.rsplit_once(':') {
        Some((f, line)) if line.chars().all(|c| c.is_ascii_digit()) => f,
        _ => file,
    };
    Some(file.to_string())
}

static IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*import\s+(?:[^'";]+?\s+from\s+)?['"]([^'"]+)['"]"#).unwrap());

static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:type\s+)?(\w+)(?:\s+as\s+\w+)?\s*$").unwrap());

/// Local modules imported by `file` (relative to the app dir), resolved to existing .ts/.js files, with the names
/// imported in braces (`None` when the whole module is imported: default or `* as`).
fn local_imports(dir: &Path, file: &str, text: &str) -> Vec<(String, Option<Vec<String>>)> {
    let base = Path::new(file).parent().unwrap_or(Path::new(""));
    let mut out = Vec::new();
    for cap in IMPORT.captures_iter(text) {
        let spec = &cap[1];
        let rel: PathBuf = if let Some(rest) = spec.strip_prefix("@/") {
            Config::src_rel(dir).join(rest)
        } else if spec.starts_with('.') {
            normalize(&base.join(spec))
        } else {
            continue;
        };
        let clause = cap[0].split_once("import").map(|(_, r)| r).unwrap_or_default();
        let names = match (clause.find('{'), clause.find('}')) {
            (Some(a), Some(b)) if b > a && !clause[..a].contains(',') && !clause.contains("* as") => Some(
                clause[a + 1..b]
                    .split(',')
                    .filter_map(|n| NAMED.captures(n).map(|c| c[1].to_string()))
                    .collect(),
            ),
            _ => None,
        };
        for cand in [
            rel.clone(),
            rel.with_extension("ts"),
            rel.with_extension("js"),
            rel.join("index.ts"),
            rel.join("index.js"),
        ] {
            let ext_ok = cand.extension().is_some_and(|e| e == "ts" || e == "js");
            if ext_ok && dir.join(&cand).is_file() {
                out.push((cand.to_string_lossy().into_owned(), names));
                break;
            }
        }
    }
    out
}

static EXPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*export\s+(?:default\s+)?(?:async\s+)?(?:function\s*\*?\s*(\w+)|(?:const|let)\s+(\w+))").unwrap()
});

/// The exported function or const that contains 1-based `line`.
fn enclosing_export(text: &str, line: usize) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    for l in lines.iter().take(line.min(lines.len())).rev() {
        if let Some(c) = EXPORT.captures(l) {
            return c.get(1).or(c.get(2)).map(|m| m.as_str().to_string());
        }
    }
    None
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|\s)(?:label|title|placeholder|hint|text|subtitle|tooltip)="([^"{}]*[A-Za-z][^"{}]*)""#).unwrap()
});
static TEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r">([^<>{}]*[A-Za-z][^<>{}]*)<").unwrap());

/// Text written straight into a .vue template: attribute values (label="…") and text between tags.
pub fn template_literals(vue: &str) -> Vec<String> {
    let start = vue.find("<template").unwrap_or(0);
    let end = vue
        .rfind("</template>")
        .map(|e| e + "</template>".len())
        .unwrap_or(vue.len());
    let tpl = &vue[start..end.max(start)];
    let mut out = Vec::new();
    for c in ATTR.captures_iter(tpl).chain(TEXT.captures_iter(tpl)) {
        let t = c[1].split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Validation rules in PHP source: the arrays passed to `validate(`, `Validator::make(` or returned by `rules()`.
pub fn parse_rules(src: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for anchor in ["validate(", "Validator::make(", "function rules("] {
        let mut from = 0;
        while let Some(i) = src[from..].find(anchor) {
            let at = from + i + anchor.len();
            from = at;
            let Some(block) = next_array(src, at, if anchor == "function rules(" { 400 } else { 120 }) else {
                continue;
            };
            for entry in split_top(&block, ',') {
                let Some((k, v)) = split_arrow(&entry) else { continue };
                let Some(key) = unquote(k.trim()) else { continue };
                let v = v.trim();
                let mut rules = Vec::new();
                if let Some(s) = unquote(v) {
                    rules.extend(s.split('|').filter(|r| !r.is_empty()).map(String::from));
                } else if v.starts_with('[') {
                    let inner = &v[1..v.len().saturating_sub(1)];
                    for item in split_top(inner, ',') {
                        let item = item.trim();
                        if let Some(s) = unquote(item) {
                            rules.extend(s.split('|').filter(|r| !r.is_empty()).map(String::from));
                        } else if let Some((_, rest)) = item.split_once("::") {
                            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                            if !name.is_empty() {
                                rules.push(name.to_lowercase());
                            }
                        } else if let Some(rest) = item.strip_prefix("new ") {
                            let name: String = rest
                                .chars()
                                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '\\')
                                .collect();
                            rules.push(name.rsplit('\\').next().unwrap_or_default().to_string());
                        }
                    }
                }
                if !rules.is_empty() {
                    out.insert(key, rules);
                }
            }
        }
    }
    out
}

/// The `[...]` block starting within `window` chars after `at` (contents without the brackets).
fn next_array(src: &str, at: usize, window: usize) -> Option<String> {
    let rest = &src[at..];
    let open = rest
        .char_indices()
        .take_while(|(i, _)| *i < window)
        .find(|(_, c)| *c == '[')?
        .0;
    let bytes: Vec<char> = rest[open..].chars().collect();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut out = String::new();
    for (i, &c) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if c == q && bytes.get(i.wrapping_sub(1)) != Some(&'\\') {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '[' || c == '(' {
            depth += 1;
            if depth == 1 {
                continue;
            }
        } else if c == ']' || c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(out);
            }
        }
        out.push(c);
    }
    None
}

/// Splits on `sep` outside brackets and quotes.
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut prev = ' ';
    for c in s.chars() {
        if let Some(q) = quote {
            if c == q && prev != '\\' {
                quote = None;
            }
            cur.push(c);
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
        } else if "[({".contains(c) {
            depth += 1;
            cur.push(c);
        } else if "])}".contains(c) {
            depth -= 1;
            cur.push(c);
        } else if c == sep && depth == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
        prev = c;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn split_arrow(entry: &str) -> Option<(&str, &str)> {
    let mut quote: Option<char> = None;
    let b = entry.as_bytes();
    for i in 0..b.len() {
        let c = b[i] as char;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '=' && b.get(i + 1) == Some(&b'>') {
            return Some((&entry[..i], &entry[i + 2..]));
        }
    }
    None
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    let q = s.chars().next()?;
    if (q == '\'' || q == '"') && s.len() >= 2 && s.ends_with(q) {
        Some(s[1..s.len() - 1].replace("\\'", "'"))
    } else {
        None
    }
}

static MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:['"](?:message|error)['"]\s*=>\s*|abort\(\s*\d+\s*,\s*|__\(\s*)(?:'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)")"#).unwrap()
});

/// Messages the user may see from PHP source: `'message' => '…'`, `abort(4xx, '…')`, `__('…')`.
pub fn parse_messages(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in MESSAGE.captures_iter(src) {
        let m = c
            .get(1)
            .or(c.get(2))
            .map(|m| m.as_str().replace("\\'", "'"))
            .unwrap_or_default();
        if !m.trim().is_empty() && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn quoted_values(src: &str) -> Vec<String> {
    let re = Regex::new(r#"=>\s*'((?:[^'\\]|\\.)*)'"#).unwrap();
    re.captures_iter(src).map(|c| c[1].replace("\\'", "'")).collect()
}

/// Menu entries from a menu config file: objects with `routeName` and `label`; the top-level key is the module and
/// the nearest object with `label` and `items` the group.
pub fn parse_menu(text: &str) -> Vec<MenuEntry> {
    let mut out = Vec::new();
    if let Some(Node::Map(entries, _)) = parse_tree(text) {
        for (module, node) in &entries {
            walk_menu(node, module, "", &mut out);
        }
    }
    out
}

fn walk_menu(node: &Node, module: &str, group: &str, out: &mut Vec<MenuEntry>) {
    let Node::Map(entries, elements) = node else { return };
    let label = node.get("label").and_then(Node::str).map(str::trim).unwrap_or_default();
    if let Some(route) = node.get("routeName").and_then(Node::str) {
        let aro = ["aro", "permission", "can"]
            .iter()
            .find_map(|k| node.get(k).and_then(Node::str))
            .unwrap_or_default();
        out.push(MenuEntry {
            route_name: route.to_string(),
            label: label.to_string(),
            module: module.to_string(),
            group: group.to_string(),
            aro: aro.to_string(),
        });
    }
    let group = if node.get("items").is_some() && !label.is_empty() {
        label
    } else {
        group
    };
    for (_, child) in entries {
        walk_menu(child, module, group, out);
    }
    for child in elements {
        walk_menu(child, module, group, out);
    }
}

/// sha256 of the facts in a canonical order, plus the hash of the locale files.
pub fn fingerprint(f: &PageFacts, labels_hash: &str) -> String {
    let mut c = f.clone();
    for v in c.labels.values_mut() {
        v.sort();
    }
    c.literals.sort();
    c.components.sort();
    c.menu_titles.sort();
    for e in c.endpoints.iter_mut() {
        e.middleware.sort();
        e.permissions.sort();
        e.messages.sort();
    }
    c.endpoints
        .sort_by(|a, b| (&a.method, &a.uri).cmp(&(&b.method, &b.uri)));
    let text = serde_json::to_string(&c).unwrap_or_default();
    crate::hash::sha256(&format!("{text}\n{labels_hash}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codemap::FixtureCodemap;

    pub fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// A config for the sample shop project (backend tests/fixtures/shop, frontend shop/web), in a temp dir.
    pub fn shop_config(dir: &Path) -> Config {
        let shop = fixtures().join("shop");
        let toml = format!(
            "[project]\nname=\"shop\"\nbackend=\"{b}\"\nfrontends=[\"{b}/web\"]\nlanguages=[\"en\",\"id\"]\n[guide]\nmodules_from=\"{b}/web/src/config/menu.ts\"\nallow_bold=[\"Note\"]\nrequire_actions=false\nmin_screenshots=0\n",
            b = shop.display()
        );
        std::fs::write(dir.join(".doctest.toml"), toml).unwrap();
        Config::find(dir).unwrap()
    }

    fn setup() -> (tempfile::TempDir, Config, FixtureCodemap, Labels) {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
        (d, cfg, FixtureCodemap::new(fixtures().join("codemap")), labels)
    }

    #[test]
    fn lists_pages_with_module() {
        let (_d, cfg, mut api, labels) = setup();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let pages = fb.pages().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].key, "shop.orders.create");
        assert_eq!(pages[0].module, "orders");
        assert_eq!(pages[0].app, "web");
        assert_eq!(pages[0].component, PathBuf::from("src/pages/ShopOrderForm.vue"));
    }

    #[test]
    fn pages_get_their_portal() {
        let (_d, mut cfg, mut api, labels) = setup();
        cfg.guide.portals = vec![crate::config::Portal {
            name: "shop".into(),
            title: "Shop guide".into(),
            pages: vec!["shop.*".into()],
            user: None,
            login_url: None,
        }];
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        assert_eq!(fb.pages().unwrap()[0].portal, "shop");
    }

    #[test]
    fn module_prefers_the_menu_named_in_the_path() {
        let menu = vec![
            MenuEntry {
                route_name: "admin.rems.data.lease-out".into(),
                label: "Lease Out".into(),
                module: "anpManagement".into(),
                group: "A&P".into(),
                aro: String::new(),
            },
            MenuEntry {
                route_name: "admin.rems.data.lease-out".into(),
                label: "Lease Out".into(),
                module: "rems".into(),
                group: "Tenancy Setup".into(),
                aro: String::new(),
            },
        ];
        assert_eq!(
            module_for(&menu, "admin.rems.data.lease-out", "/admin/rems/data/lease-out"),
            Some("rems".to_string())
        );
        assert_eq!(
            module_for(&menu[..1], "admin.rems.data.lease-out", "/admin/rems/data/lease-out"),
            Some("anpManagement".to_string())
        );
        assert_eq!(module_for(&menu, "x", "/x"), None);
    }

    #[test]
    fn module_follows_the_permission_the_page_checks() {
        let (_d, cfg, mut api, labels) = setup();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        fb.menu = vec![
            MenuEntry {
                route_name: "shop.orders.create".into(),
                label: "New Order".into(),
                module: "marketing".into(),
                group: "Ads".into(),
                aro: "ads/booking".into(),
            },
            MenuEntry {
                route_name: "shop.orders.create".into(),
                label: "New Order".into(),
                module: "orders".into(),
                group: "Orders".into(),
                aro: "orders/create".into(),
            },
        ];
        let mut page = fb.pages().unwrap().remove(0);
        page.module = "marketing".into();
        let f = fb.facts(&page).unwrap();
        assert_eq!(f.page.module, "orders");
    }

    #[test]
    fn facts_resolve_labels_and_literals() {
        let (_d, cfg, mut api, labels) = setup();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let page = fb.pages().unwrap().remove(0);
        let f = fb.facts(&page).unwrap();
        assert!(f.labels["en"].contains(&"Tenant".to_string()), "{:?}", f.labels);
        assert!(f.labels["en"].contains(&"New Order".to_string()));
        assert!(f.labels["id"].contains(&"Penyewa".to_string()));
        for lit in ["Save", "Remarks", "Delete order", "name@example.com"] {
            assert!(
                f.literals.contains(&lit.to_string()),
                "{lit} missing from {:?}",
                f.literals
            );
        }
        assert_eq!(f.menu_titles, vec!["Orders".to_string(), "New Order".to_string()]);
    }

    #[test]
    fn facts_endpoint_rules() {
        let (_d, cfg, mut api, labels) = setup();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let page = fb.pages().unwrap().remove(0);
        let f = fb.facts(&page).unwrap();
        assert_eq!(f.endpoints.len(), 1);
        let e = &f.endpoints[0];
        assert_eq!((e.method.as_str(), e.uri.as_str()), ("POST", "api/orders/{order}"));
        assert_eq!(e.action, "App\\Http\\Controllers\\OrderController@store");
        assert_eq!(e.permissions, vec!["orders/create"]);
        assert!(e.middleware.contains(&"auth:sanctum".to_string()));
        assert_eq!(e.rules["email"], vec!["required", "email", "max:255"]);
        assert_eq!(e.rules["qty"], vec!["required", "integer", "min:1"]);
        assert!(!e.rules.contains_key("status"));
        assert!(e.messages.contains(&"Order is already closed.".to_string()));
    }

    #[test]
    fn only_imported_functions_count() {
        // ShopOrderForm imports { useOrderForm } from orders.ts; deleteEverything's call must not be a page endpoint.
        let (_d, cfg, mut api, labels) = setup();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let page = fb.pages().unwrap().remove(0);
        let f = fb.facts(&page).unwrap();
        assert!(
            f.endpoints.iter().all(|e| e.uri != "api/everything"),
            "{:?}",
            f.endpoints
        );
    }

    fn ep(method: &str, uri: &str, messages: &[&str], rules: &[(&str, &str)]) -> Endpoint {
        Endpoint {
            method: method.into(),
            uri: uri.into(),
            messages: messages.iter().map(|m| m.to_string()).collect(),
            rules: rules
                .iter()
                .map(|(k, v)| (k.to_string(), v.split('|').map(String::from).collect()))
                .collect(),
            ..Endpoint::default()
        }
    }

    #[test]
    fn actions_from_endpoints() {
        let mut f = sample();
        f.endpoints = vec![
            ep("GET", "api/orders", &[], &[]),
            ep(
                "POST",
                "api/orders",
                &["Order created successfully."],
                &[("email", "required|email"), ("note", "nullable")],
            ),
            ep("PUT", "api/orders/{order}", &["Order updated."], &[]),
            ep(
                "DELETE",
                "api/orders/{order}",
                &["Order deleted.", "Order is in use."],
                &[],
            ),
            ep("POST", "api/orders/{order}/approve", &[], &[]),
            ep("GET", "api/orders/export", &[], &[]),
        ];
        let a = actions(&f);
        let names: Vec<&str> = a.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["Add", "Edit", "Delete", "Approve", "Export"]);
        assert_eq!(a[0].required, vec!["email"]);
        assert_eq!(a[0].fields, vec!["email", "note"]);
        assert_eq!(a[0].success, vec!["Order created successfully."]);
        assert_eq!(a[2].success, vec!["Order deleted."]);
        assert_eq!(a[2].errors, vec!["Order is in use."]);
        let plan = screenshot_plan(&a);
        assert!(plan.len() > a.len());
        assert!(
            plan.iter().any(|s| s.contains("Delete") && s.contains("confirm")),
            "{plan:?}"
        );
        assert!(plan.iter().any(|s| s.contains("validation error")), "{plan:?}");
    }

    #[test]
    fn enclosing_export_finds_function() {
        let t = "export function a() {\n  x()\n}\nexport const b = () => {\n  y()\n}\n";
        assert_eq!(enclosing_export(t, 2).as_deref(), Some("a"));
        assert_eq!(enclosing_export(t, 5).as_deref(), Some("b"));
    }

    #[test]
    fn parse_rules_string_and_array_forms() {
        let src = "$v = Validator::make($request->all(), [\n 'name' => 'required|string',\n \"code\" => ['nullable', Rule::in(['a'])],\n]);";
        let r = parse_rules(src);
        assert_eq!(r["name"], vec!["required", "string"]);
        assert_eq!(r["code"], vec!["nullable", "in"]);
    }

    #[test]
    fn menu_entries_from_ts() {
        let text = std::fs::read_to_string(fixtures().join("shop/web/src/config/menu.ts")).unwrap();
        let m = parse_menu(&text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].route_name, "shop.orders.create");
        assert_eq!(m[0].module, "orders");
        assert_eq!(m[0].group, "Orders");
        assert_eq!(m[0].label, "New Order");
    }

    fn sample() -> PageFacts {
        PageFacts {
            page: Page {
                key: "k".into(),
                app: "web".into(),
                path: "/k".into(),
                route_name: None,
                component: "a.vue".into(),
                module: "m".into(),
                portal: String::new(),
            },
            labels: BTreeMap::from([("en".to_string(), vec!["B".to_string(), "A".to_string()])]),
            literals: vec!["Save".into()],
            components: vec!["a.vue".into()],
            code_files: vec![],
            endpoints: vec![],
            menu_titles: vec![],
        }
    }

    #[test]
    fn fingerprint_changes_with_label_file() {
        let f = sample();
        assert_ne!(fingerprint(&f, "h1"), fingerprint(&f, "h2"));
    }

    #[test]
    fn fingerprint_stable_under_reordering() {
        let a = sample();
        let mut b = sample();
        b.labels.get_mut("en").unwrap().reverse();
        assert_eq!(fingerprint(&a, "h"), fingerprint(&b, "h"));
    }
}
