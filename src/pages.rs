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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// Route name, else path.
    pub key: String,
    pub app: String,
    pub path: String,
    pub route_name: Option<String>,
    /// Relative to the app.
    pub component: PathBuf,
    pub module: String,
    /// The guide (audience) it belongs to; "" without portals, "public" outside every portal.
    #[serde(default)]
    pub portal: String,
    /// vue (also Vue 2 / Nuxt router pages), filament, blade; "" = vue.
    #[serde(default)]
    pub stack: String,
    /// The props its route passes (`{kind: "department"}`), read from the running app's router: a shared screen's
    /// kind picks the API it calls.
    #[serde(default)]
    pub props: BTreeMap<String, String>,
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
    /// Only reachable through shared code (a shared screen, a layout, a composable or API module): the page may
    /// not offer it. Its action counts only when the page shows a matching button.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub possible: bool,
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
    /// What the page's templates show (their text and the i18n keys they use), with what boolean props hide left
    /// out: the evidence that a button is on screen. Empty when unknown (then labels and literals count).
    #[serde(default)]
    pub screen_texts: Vec<String>,
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
    /// The portals in use: from .doctest.toml, else detected from the login pages (filled by `pages`).
    pub portals: Vec<crate::config::Portal>,
    /// Filament page key → (codemap meta, resource file).
    filament: HashMap<String, (String, String)>,
    /// (app, component file) → how many pages render it as their screen.
    page_components: HashMap<(String, PathBuf), usize>,
    /// Module key (see `module_key`) → the spelling every page of it uses (filled by `pages`).
    module_names: HashMap<String, String>,
    /// The running app's routes, once read.
    live: Option<Option<Vec<Value>>>,
}

impl<'a> FactBuilder<'a> {
    /// A message given as an i18n key (`recordSuccessfullyUploaded`, `messages.saved`) → its text in the primary
    /// language, when a locale file has it.
    fn message_text(&self, m: &str) -> String {
        let is_key = !m.contains(' ') && m.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !is_key {
            return m.to_string();
        }
        self.labels
            .resolve(self.cfg.primary_language(), m)
            .map(String::from)
            .unwrap_or_else(|| m.to_string())
    }

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
            portals: cfg.guide.portals.clone(),
            filament: HashMap::new(),
            page_components: HashMap::new(),
            module_names: HashMap::new(),
            live: None,
        }
    }

    /// The running app's routes (see `shots::live_routes`), kept in a cache until a route file changes; None when
    /// the app has no base_url, is not running and nothing is cached, or `[app] live_routes = false`.
    fn live_routes(&mut self) -> Option<Vec<Value>> {
        if let Some(r) = &self.live {
            return r.clone();
        }
        let r = live_routes_cached(self.cfg);
        self.live = Some(r.clone());
        r
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

    /// Every screen of every app: Vue / Nuxt router pages of the frontends, Filament pages and Blade pages (web
    /// routes that render a view) of the backend. Also settles the portals and each page's portal.
    pub fn pages(&mut self) -> Result<Vec<Page>, String> {
        let mut all: Vec<Page> = Vec::new();
        let push = |all: &mut Vec<Page>, p: Page| {
            if !all.iter().any(|o| o.key == p.key) {
                all.push(p);
            }
        };
        for fe in self.cfg.frontends.clone() {
            let app = Config::repo_name(&fe);
            let (items, stale) = self.api.paged(&app, "list_pages", json!({"detail": "full"}), "pages")?;
            self.stale |= stale;
            for p in items {
                let Some(file) = p["file"].as_str() else { continue };
                if p["redirect"].as_str().is_some_and(|r| !r.is_empty())
                    || !fe.join(file).is_file()
                    || is_catch_all(p["path"].as_str().unwrap_or_default())
                {
                    continue;
                }
                let path = p["path"].as_str().unwrap_or_default().to_string();
                let route_name = p["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
                let key = route_name.clone().unwrap_or_else(|| path.clone());
                let module = module_for(&self.menu, route_name.as_deref().unwrap_or_default(), &path)
                    .unwrap_or_else(|| module_from_path(&path));
                let stack = if p["source"] == "nuxt" { "nuxt" } else { "vue" };
                push(
                    &mut all,
                    Page {
                        key,
                        app: app.clone(),
                        path,
                        route_name,
                        component: PathBuf::from(file),
                        module,
                        portal: String::new(),
                        stack: stack.into(),
                        props: Default::default(),
                    },
                );
            }
        }
        // routes the running app's router registers that its route files do not show (built by helper functions),
        // and every route's props
        if let Some(live) = self.live_routes() {
            let frontends = self.cfg.frontends.clone();
            let mut by_name: HashMap<String, (String, String)> = HashMap::new();
            for r in live {
                let path = r["path"].as_str().unwrap_or_default().to_string();
                if r["redirect"] == true || r["alias"] == true || path.is_empty() || is_catch_all(&path) {
                    continue;
                }
                let route_name = r["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
                let key = route_name.clone().unwrap_or_else(|| path.clone());
                let props: BTreeMap<String, String> = r["props"]
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let found = r["file"].as_str().and_then(|f| app_file(&frontends, f)).or_else(|| {
                    let name = r["component"].as_str()?;
                    if by_name.is_empty() {
                        by_name = vue_files_by_name(&frontends);
                    }
                    by_name.get(name).cloned()
                });
                if let Some(known) = all.iter_mut().find(|o| o.key == key) {
                    known.props = props;
                    // the router knows which component a route shows; the route files can be misread
                    if let Some((app, file)) = found {
                        known.app = app;
                        known.component = PathBuf::from(file);
                    }
                    continue;
                }
                let Some((app, file)) = found else { continue };
                let module = module_for(&self.menu, route_name.as_deref().unwrap_or_default(), &path)
                    .unwrap_or_else(|| module_from_path(&path));
                push(
                    &mut all,
                    Page {
                        key,
                        app,
                        path,
                        route_name,
                        component: PathBuf::from(file),
                        module,
                        portal: String::new(),
                        stack: "vue".into(),
                        props,
                    },
                );
            }
        }
        // the backend's own screens: Filament pages
        let (items, stale) = self
            .api
            .paged(BACKEND, "list_pages", json!({"detail": "full"}), "pages")?;
        self.stale |= stale;
        for p in items.iter().filter(|p| p["source"] == "filament") {
            let Some(file) = p["file"].as_str() else { continue };
            let path = p["path"].as_str().unwrap_or_default().to_string();
            let name = p["name"].as_str().unwrap_or_default().to_string();
            // filament.<panel>.resources.<resource>.<page>
            let parts: Vec<&str> = name.split('.').collect();
            let module = parts
                .iter()
                .position(|x| *x == "resources" || *x == "pages")
                .and_then(|i| parts.get(i + 1))
                .map(|s| s.to_string())
                .unwrap_or_else(|| module_from_path(&path));
            let resource_file = p["defined_in"]
                .as_str()
                .and_then(|d| d.rsplit_once(':').map(|(f, _)| f.to_string()).or(Some(d.to_string())))
                .unwrap_or_default();
            self.filament.insert(
                name.clone(),
                (p["meta"].as_str().unwrap_or_default().to_string(), resource_file),
            );
            push(
                &mut all,
                Page {
                    key: name.clone(),
                    app: BACKEND.into(),
                    path,
                    route_name: Some(name),
                    component: PathBuf::from(file),
                    module,
                    portal: String::new(),
                    stack: "filament".into(),
                    props: Default::default(),
                },
            );
        }
        // Blade pages: GET web routes whose action renders a view
        let routes: Vec<Value> = self.all_routes()?.values().cloned().collect();
        let mut routes = routes;
        routes.sort_by(|a, b| a["uri"].as_str().cmp(&b["uri"].as_str()));
        for r in routes {
            let uri = r["uri"].as_str().unwrap_or_default();
            if !r["methods"].as_str().unwrap_or_default().split('|').any(|m| m == "GET")
                || uri.starts_with("api/")
                || uri == "api"
            {
                continue;
            }
            let Some(view) = view_of(&r) else { continue };
            let file = format!("resources/views/{}.blade.php", view.replace('.', "/"));
            if !self.cfg.backend.join(&file).is_file() {
                continue;
            }
            let path = format!("/{}", uri.trim_start_matches('/'));
            let route_name = r["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
            let key = route_name.clone().unwrap_or_else(|| path.clone());
            let module = module_from_path(&path);
            push(
                &mut all,
                Page {
                    key,
                    app: BACKEND.into(),
                    path,
                    route_name,
                    component: PathBuf::from(file),
                    module,
                    portal: String::new(),
                    stack: "blade".into(),
                    props: Default::default(),
                },
            );
        }
        self.page_components.clear();
        for p in &all {
            *self
                .page_components
                .entry((p.app.clone(), p.component.clone()))
                .or_default() += 1;
        }
        if self.portals.is_empty() {
            let screens: Vec<crate::portals::Screen> = all
                .iter()
                .map(|p| crate::portals::Screen {
                    path: p.path.clone(),
                    name: p.key.clone(),
                    stack: p.stack.clone(),
                })
                .collect();
            self.portals = crate::portals::detect(&screens);
        }
        let g = &self.cfg.guide;
        let mut out: Vec<Page> = all
            .into_iter()
            .filter(|p| {
                (g.include.is_empty() || crate::work::page_matches(&p.key, &p.path, &g.include))
                    && !crate::work::page_matches(&p.key, &p.path, &g.exclude)
            })
            // a parent route whose component only holds its child pages has nothing to document or shoot
            .filter(|p| {
                let file = self.app_dir(&p.app).join(&p.component);
                !(file.extension().is_some_and(|x| x == "vue")
                    && std::fs::read_to_string(&file).is_ok_and(|t| is_container(&t)))
            })
            .map(|mut p| {
                p.portal = crate::portals::portal_in(&self.portals, &p.key, &p.path);
                p
            })
            .collect();
        // two routes with the same path: only the first registered one ever shows at that URL
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        out.retain(|p| seen.insert((p.app.clone(), same_path(&p.path))));
        // one module, one folder: names that differ only in punctuation (eform, e-form) take the commonest spelling
        let mut spellings: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for p in &out {
            *spellings
                .entry(module_key(&p.module))
                .or_default()
                .entry(p.module.clone())
                .or_default() += 1;
        }
        for p in out.iter_mut() {
            if let Some(best) = spellings.get(&module_key(&p.module)).and_then(|v| {
                v.iter()
                    .max_by_key(|(name, n)| (**n, name.contains('-'), std::cmp::Reverse((*name).clone())))
                    .map(|(name, _)| name.clone())
            }) {
                self.module_names.insert(module_key(&best), best.clone());
                p.module = best;
            }
        }
        out.sort_by(|a, b| (&a.module, &a.key).cmp(&(&b.module, &b.key)));
        Ok(out)
    }

    pub fn facts(&mut self, page: &Page) -> Result<PageFacts, String> {
        match page.stack.as_str() {
            "filament" => return Ok(self.filament_facts(page)),
            "blade" => return self.blade_facts(page),
            _ => {}
        }
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
        // boolean props a parent passes to a child (`<BookingList approval />`): what they hide is not on this page
        let mut passed: HashMap<String, BTreeMap<String, bool>> = HashMap::new();
        // string props (`kind`) from the route down through the components that pass them on
        let mut strings: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        strings.insert(page.component.to_string_lossy().into_owned(), page.props.clone());
        let mut screen: BTreeSet<String> = BTreeSet::new();
        // the page's templates, other kinds' parts left out: which composable functions it wires up
        let mut templates = String::new();
        let mut vue_scripts = String::new();
        // files whose calls are certainly this page's: its own components, not a shared screen or a layout
        let mut certain_files: BTreeSet<String> = BTreeSet::new();
        let mut uncertain: BTreeSet<String> = BTreeSet::new();
        while let Some((file, depth)) = queue.pop() {
            if components.contains(&file) {
                continue;
            }
            components.push(file.clone());
            code_files.insert(file.clone());
            let full_text = std::fs::read_to_string(dir.join(&file)).unwrap_or_default();
            let no_strings = BTreeMap::new();
            let strs = strings.get(&file).unwrap_or(&no_strings).clone();
            let text = match passed.get(&file) {
                Some(flags) if !flags.is_empty() || !strs.is_empty() => prune_template(&full_text, flags, &strs),
                _ if !strs.is_empty() => prune_template(&full_text, &BTreeMap::new(), &strs),
                _ => full_text.clone(),
            };
            literals.extend(template_literals(&text));
            literals.extend(script_literals(&text));
            if file.ends_with(".vue") {
                templates.push_str(template_part(&text));
                templates.push('\n');
                // functions the component's script calls (`useOrderForm(props)`), not ones it only destructures
                vue_scripts.push_str(&text.replacen(template_part(&text), "", 1));
                vue_scripts.push('\n');
            }
            // i18n keys the code names in a string anywhere (a lookup map, `cond ? 'a.b' : 'a.c'`, a helper's
            // argument): their texts can be on screen even when codemap does not see a t() call for them
            for key in quoted_keys(&text) {
                for (lang, set) in labels.iter_mut() {
                    if let Some(t) = self.labels.text(lang, &key) {
                        set.insert(t.trim().to_string());
                    }
                }
            }
            let pages_using = self
                .page_components
                .get(&(page.app.clone(), PathBuf::from(&file)))
                .copied()
                .unwrap_or(0);
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
            let used_by = c["used_by"].as_array().map(Vec::len).unwrap_or(0);
            // what this page shows; a generic component used all over (a list or form shell) shows what its props
            // ask for, so its own texts are no evidence of a button here
            if used_by >= 3 {
                // a generic shell asked for a button by a prop (`:new-to="'/x'"`, `:show-delete="true"`)
                for (prop, on) in passed.get(&file).into_iter().flatten() {
                    let p = prop.to_lowercase();
                    let word = [
                        ("new", "New"),
                        ("create", "New"),
                        ("add", "Add"),
                        ("delete", "Delete"),
                        ("remove", "Delete"),
                        ("export", "Export"),
                        ("import", "Import"),
                        ("print", "Print"),
                        ("download", "Download"),
                    ]
                    .iter()
                    .find(|(k, _)| p.contains(k))
                    .map(|(_, w)| *w);
                    if let (true, Some(w)) = (*on, word) {
                        screen.insert(w.to_string());
                    }
                }
            } else {
                screen.extend(template_literals(&text));
                for key in quoted_keys(template_part(&text)) {
                    if let Some(t) = self.labels.text(self.cfg.primary_language(), &key) {
                        screen.insert(t.trim().to_string());
                    }
                }
            }
            let under_shared = uncertain.contains(&file);
            if under_shared || is_layout(&text) || is_shared(pages_using, used_by) {
                uncertain.insert(file.clone());
            } else {
                certain_files.insert(file.clone());
            }
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
                // three levels of the page's own parts; frames that wrap content (a <slot>) are followed further,
                // so the labels of a layout reached through several frames (Switch, Log out) are the page's too
                if let Some(target) = arrow_target(r)
                    && (depth < 3 || (depth < 6 && text.contains("<slot")))
                    && dir.join(&target).is_file()
                {
                    // what a layout or shared screen renders is shared too
                    if uncertain.contains(&file) {
                        uncertain.insert(target.clone());
                    }
                    // `<OrderCustomer> :4 → src/…` → OrderCustomer
                    let tag = r
                        .split('→')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .trim_start_matches('<')
                        .split(['>', ' ', ':'])
                        .next()
                        .unwrap_or_default();
                    // a child only another kind's part of the template renders is not on this page
                    if !renders_tag(template_part(&text), tag) && template_part(&full_text) != full_text {
                        continue;
                    }
                    let child_strs = static_strings(&text, tag, &strs);
                    if !child_strs.is_empty() {
                        strings.entry(target.clone()).or_default().extend(child_strs);
                    }
                    let flags = static_flags(&text, tag, passed.get(&file).unwrap_or(&BTreeMap::new()));
                    if !flags.is_empty() {
                        passed.entry(target.clone()).or_default().extend(flags);
                    }
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
        let mut guesses: BTreeSet<(String, String)> = BTreeSet::new();
        let mut wired_cache: HashMap<String, (BTreeSet<String>, BTreeSet<String>)> = HashMap::new();
        // the page's own code (its components and composables), to tell which functions of a shared module it calls
        // (branches for another kind left out: `props.kind === 'customer' ? customerApi.list() : …`)
        let own_code: String = code_files
            .iter()
            .filter_map(|f| std::fs::read_to_string(dir.join(f)).ok())
            .map(|t| prune_script(&t, &page.props))
            .collect::<Vec<_>>()
            .join("\n");
        let scan_files_list: Vec<String> = imported.keys().cloned().collect();
        let imported_names = imported.clone();
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
            let file_text = std::fs::read_to_string(dir.join(&f)).unwrap_or_default();
            for call in calls {
                let Some(at) = call["at"].as_str() else { continue };
                if call["status"] != "matched" || !at.starts_with(&format!("{f}:")) {
                    continue;
                }
                // a shared API module's call counts only when the page's own code calls that function or method
                if !code_files.contains(&f)
                    && let Some(member) = enclosing_member(&file_text, line_no_of(at))
                {
                    // a method of an exported object (`customerApi.list`) is called through that object
                    let object = enclosing_export(&file_text, line_no_of(at)).filter(|o| *o != member);
                    let called = match object {
                        Some(o) if own_code.contains(o.as_str()) => Regex::new(&format!(
                            r"\b{}\s*\??\.\s*{}\b",
                            regex::escape(&o),
                            regex::escape(&member)
                        ))
                        .is_ok_and(|r| r.is_match(&own_code)),
                        _ => mentions_call(&own_code, &member),
                    };
                    if !called {
                        continue;
                    }
                }
                // a call whose URL starts with a value only known at run time (`${contract.endpoint}/x`,
                // `base + '/x'`) was matched to a route by its ending: a guess
                let line_no: usize = at.rsplit(':').next().and_then(|l| l.parse().ok()).unwrap_or(0);
                let call_lines: String = file_text
                    .lines()
                    .skip(line_no.saturating_sub(1))
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n");
                // …unless the run-time part is a constant of the same file that gives this very route
                let route_uri = call["route"]
                    .as_str()
                    .and_then(|r| r.split_once(' '))
                    .map(|(_, u)| u)
                    .unwrap_or("");
                let guessed = DYNAMIC_URL.is_match(&call_lines)
                    && !literal_paths(&call_lines, &file_text, &[])
                        .iter()
                        .any(|p| path_matches(p, route_uri));
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
                // a composable function this page never uses (`signOff`, wired only in another kind's dialog): its
                // calls are not this page's
                if !f.ends_with(".vue")
                    && code_files.contains(&f)
                    && !method.is_empty()
                    && let Some(m) = enclosing_member(&file_text, line_no)
                {
                    let wired = wired_cache
                        .entry(f.clone())
                        .or_insert_with(|| wired_functions(&file_text, &templates, &vue_scripts));
                    if wired.0.contains(&m) && !wired.1.contains(&m) {
                        continue;
                    }
                }
                let possible = guessed || !certain_files.contains(&f);
                if guessed {
                    guesses.insert((method.clone(), uri.to_string()));
                }
                if seen.insert((method.clone(), uri.to_string())) {
                    if let Some(mut e) = self.endpoint(&method, uri)? {
                        e.possible = possible;
                        endpoints.push(e);
                    }
                } else if !possible && let Some(e) = endpoints.iter_mut().find(|e| e.method == method && e.uri == uri) {
                    e.possible = false;
                    guesses.remove(&(method.clone(), uri.to_string()));
                }
            }
        }
        // API paths built at run time (`${contract.endpoint}/${id}/approve`, a per-kind contract object) are not calls
        // codemap can match. The object that names this page (its route name, as contracts do with `detailRoute`, or
        // its kind from the page key: `'department': {...}`) gives the base paths; the page's own files are read
        // whole. Template paths that start with a field of that object (`${c.endpoint}/…`) are joined to its value.
        // These calls are possible, not certain.
        // the kind this page is (tried last to first): the last word of its key, the kinds its own code passes to an
        // API call (`downloadReport('asset-fsr', …)`), and its route's props (`{kind: "department"}`). Earlier key
        // words (`asset` in admin.asset.report.x) name the module, not the kind.
        let mut hints: Vec<String> = page
            .key
            .split(['.', '/'])
            .rfind(|w| !w.is_empty() && !GENERIC_KEY_WORDS.contains(w))
            .map(|w| vec![w.to_string()])
            .unwrap_or_default();
        for k in call_kinds(&own_code) {
            if !hints.contains(&k) {
                hints.push(k);
            }
        }
        for v in page.props.values() {
            if !v.is_empty() && !v.contains(' ') && !hints.contains(v) {
                hints.push(v.clone());
            }
        }
        let texts: Vec<(String, String)> = code_files
            .iter()
            .chain(scan_files_list.iter())
            .filter_map(|f| std::fs::read_to_string(dir.join(f)).ok().map(|t| (f.clone(), t)))
            .collect();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let mut candidates: Vec<String> = Vec::new();
        let mut anchored_any = false;
        let mut kind_bases: Vec<String> = Vec::new();
        let mut sibling_bases: BTreeSet<String> = BTreeSet::new();
        for (f, text) in &texts {
            let anchored = page
                .route_name
                .as_deref()
                .and_then(|r| block_naming(text, r))
                .or_else(|| hints.iter().rev().find_map(|h| keyed_block(text, h)));
            for h in hints.iter().rev() {
                let values = keyed_values(text, h);
                if !values.is_empty() {
                    anchored_any = true;
                    kind_bases.extend(values.iter().cloned());
                    candidates.extend(values);
                    break;
                }
            }
            if let Some(region) = anchored {
                anchored_any = true;
                let own = path_fields(region);
                // the same fields of the other contracts in this file (`endpoint:` of the access-card contract)
                for (name, value) in path_values_named(text, &own.keys().cloned().collect::<Vec<_>>()) {
                    if own.get(&name) != Some(&value) {
                        sibling_bases.insert(value);
                    }
                }
                fields.extend(own);
                candidates.extend(literal_paths(region, text, &hints));
            } else if certain_files.contains(f) {
                candidates.extend(literal_paths(text, text, &hints));
            } else if let Some(Some(names)) = imported_names.get(f) {
                // an API module the page imports functions from by name: the paths inside those functions
                candidates.extend(literal_paths_in(text, names, &hints, &own_code));
            }
        }
        if !fields.is_empty() || !kind_bases.is_empty() {
            for (_, text) in &texts {
                candidates.extend(joined_paths(text, &fields));
                candidates.extend(indexed_paths(text, &kind_bases));
            }
        }
        let mut literal_hits: Vec<(String, String)> = Vec::new();
        if !candidates.is_empty() {
            let routes = self.all_routes()?;
            for lit in &candidates {
                let mut hits: Vec<(String, String)> = routes
                    .keys()
                    .filter(|(m, uri)| m != "HEAD" && path_matches(lit, uri))
                    .cloned()
                    .collect();
                hits.sort();
                literal_hits.extend(hits);
            }
        }
        // the page's own contract (the object that names it or its kind) was found and matched: what a shared module
        // offers besides it, and what codemap matched by a URL's ending, is not this page's
        let contract: BTreeSet<(String, String)> = if anchored_any {
            literal_hits.iter().cloned().collect()
        } else {
            BTreeSet::new()
        };
        if !contract.is_empty() {
            let own_bases: Vec<&String> = fields.values().chain(kind_bases.iter()).collect();
            endpoints.retain(|e| {
                let k = (e.method.clone(), e.uri.clone());
                // an address under another kind's base (the longest base that fits decides)
                let uri = e.uri.strip_prefix("api/").unwrap_or(&e.uri);
                let under = |b: &&String| uri == b.as_str() || uri.starts_with(&format!("{b}/"));
                let other = sibling_bases.iter().filter(under).map(|b| b.len()).max().unwrap_or(0);
                let mine = own_bases
                    .iter()
                    .filter(|b| under(b))
                    .map(|b| b.len())
                    .max()
                    .unwrap_or(0);
                contract.contains(&k) || (!guesses.contains(&k) && other <= mine)
            });
        }
        for (method, uri) in literal_hits {
            if seen.insert((method.clone(), uri.clone()))
                && let Some(mut e) = self.endpoint(&method, &uri)?
            {
                e.possible = true;
                endpoints.push(e);
            }
        }
        // shared code (a layout, a shell) can reach another portal's API: those calls are not this page's
        let others: Vec<String> = self
            .portals
            .iter()
            .map(|p| p.name.to_lowercase())
            .filter(|n| !page.portal.is_empty() && *n != page.portal.to_lowercase() && n != "public")
            .collect();
        endpoints.retain(|e| !(e.possible && of_other_portal(&e.uri, &others)));
        // a page in several menus belongs to the one whose permission its endpoints check
        let mut page = page.clone();
        if let Some(r) = page.route_name.as_deref() {
            let perms: Vec<&String> = endpoints.iter().flat_map(|e| e.permissions.iter()).collect();
            if let Some(m) = self
                .menu
                .iter()
                .find(|m| m.route_name == r && !m.aro.is_empty() && perms.contains(&&m.aro))
            {
                page.module = self
                    .module_names
                    .get(&module_key(&m.module))
                    .cloned()
                    .unwrap_or_else(|| m.module.clone());
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
            screen_texts: screen.into_iter().collect(),
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

    fn empty_labels(&self) -> BTreeMap<String, Vec<String>> {
        self.cfg.languages.iter().map(|l| (l.clone(), Vec::new())).collect()
    }

    /// A Filament page: its table columns and form fields (as Filament shows them) and the labels in its resource.
    fn filament_facts(&mut self, page: &Page) -> PageFacts {
        let (meta, resource) = self.filament.get(&page.key).cloned().unwrap_or_default();
        let mut literals: BTreeSet<String> = BTreeSet::new();
        for part in meta.split(" · ") {
            if let Some((_, fields)) = part.split_once("(): ") {
                for f in fields.split(',').map(str::trim).filter(|f| !f.is_empty() && *f != "id") {
                    literals.insert(headline(f));
                }
            }
        }
        let mut code_files = vec![page.component.to_string_lossy().into_owned()];
        if !resource.is_empty() {
            if let Ok(text) = std::fs::read_to_string(self.cfg.backend.join(&resource)) {
                static LABEL: LazyLock<Regex> = LazyLock::new(|| {
                    Regex::new(r#"->(?:label|heading|title|placeholder|helperText)\(\s*'([^']+)'"#).unwrap()
                });
                static MAKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"::make\(\s*'([\w.]+)'"#).unwrap());
                literals.extend(LABEL.captures_iter(&text).map(|c| c[1].to_string()));
                literals.extend(MAKE.captures_iter(&text).map(|c| headline(&c[1])));
            }
            code_files.push(resource.clone());
        }
        PageFacts {
            page: page.clone(),
            labels: self.empty_labels(),
            literals: literals.into_iter().collect(),
            components: code_files.clone(),
            code_files,
            endpoints: Vec::new(),
            menu_titles: vec![headline(&page.module)],
            screen_texts: Vec::new(),
        }
    }

    /// A Blade page: its text, its translated labels (`__()`, `@lang`, `trans`), the views it includes and the
    /// routes its forms and links use.
    fn blade_facts(&mut self, page: &Page) -> Result<PageFacts, String> {
        static INCLUDE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"@include(?:If|When)?\(\s*(?:[^,]+,\s*)?'([\w.\-/]+)'"#).unwrap());
        static LANG: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"(?:__|trans|trans_choice|@lang)\(\s*['"]([^'"]+)['"]"#).unwrap());
        static ROUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"route\(\s*['"]([\w.\-]+)['"]"#).unwrap());
        let mut labels: BTreeMap<String, BTreeSet<String>> = self
            .cfg
            .languages
            .iter()
            .map(|l| (l.clone(), BTreeSet::new()))
            .collect();
        let mut literals = BTreeSet::new();
        let mut files = vec![page.component.to_string_lossy().into_owned()];
        let mut route_names = BTreeSet::new();
        let mut i = 0;
        while i < files.len() && i < 20 {
            let text = std::fs::read_to_string(self.cfg.backend.join(&files[i])).unwrap_or_default();
            literals.extend(template_literals(&text));
            for c in LANG.captures_iter(&text) {
                for (lang, set) in labels.iter_mut() {
                    if let Some(t) = self.labels.text(lang, &c[1]) {
                        set.insert(t.trim().to_string());
                    }
                }
            }
            route_names.extend(ROUTE.captures_iter(&text).map(|c| c[1].to_string()));
            for c in INCLUDE.captures_iter(&text) {
                let f = format!("resources/views/{}.blade.php", c[1].replace('.', "/"));
                if !files.contains(&f) && self.cfg.backend.join(&f).is_file() {
                    files.push(f);
                }
            }
            i += 1;
        }
        let mut endpoints = Vec::new();
        let named: Vec<(String, String)> = self
            .all_routes()?
            .iter()
            .filter(|(_, r)| r["name"].as_str().is_some_and(|n| route_names.contains(n)))
            .map(|((m, u), _)| (m.clone(), u.clone()))
            .filter(|(m, _)| m != "GET" && m != "HEAD")
            .collect();
        for (m, u) in named {
            if let Some(e) = self.endpoint(&m, &u)? {
                endpoints.push(e);
            }
        }
        Ok(PageFacts {
            page: page.clone(),
            labels: labels.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
            literals: literals.into_iter().collect(),
            components: files.clone(),
            code_files: files,
            endpoints,
            menu_titles: Vec::new(),
            screen_texts: Vec::new(),
        })
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
                e.messages = parse_messages(&src)
                    .into_iter()
                    .map(|m| self.message_text(&m))
                    .collect();
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
    /// Reached only through shared code, or a guess: a guide needs it only if the page shows its control.
    #[serde(default)]
    pub possible: bool,
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
    // "post" and "copy" are also nouns at the start of a name (post-evaluation, copy-of-lease)
    let named = NAMED_ACTIONS.iter().find(|n| {
        last_literal == **n
            || last_literal.ends_with(&format!("-{n}"))
            || (!matches!(**n, "post" | "copy") && last_literal.starts_with(&format!("{n}-")))
    });
    let reads = matches!(e.method.as_str(), "GET" | "HEAD");
    // a GET only reads: of the named actions only the ones that hand out a file
    let named = named.filter(|n| !reads || matches!(**n, "export" | "print" | "download" | "pdf" | "xls" | "excel"));
    // a POST to …/delete-receipt/{id}, or one whose success message says deleted, deletes
    let deletes = !reads
        && (segs.iter().filter(|s| !s.starts_with('{')).any(|s| {
            ["delete", "remove", "destroy"]
                .iter()
                .any(|w| s.to_lowercase().contains(w))
        }) || e
            .messages
            .iter()
            .any(|m| reads_as_success(m) && m.to_lowercase().contains("delet")));
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
        (_, None) if deletes => Some("Delete".into()),
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

/// Does this API path belong to one of these other portals (a path segment named after it, like api/selfcare/…)?
pub fn of_other_portal(uri: &str, others: &[String]) -> bool {
    // whole path parts only: `selfcare-menu-configuration` is an admin page's API, `api/selfcare/…` is not
    uri.to_lowercase().split('/').any(|seg| others.iter().any(|o| o == seg))
}

/// Does the page show the API's own message (`response.data.message`, `res.message`)? Many pages show their own text
/// after a save instead; then the API's success message is never on screen. Unknown code counts as yes.
pub fn shows_api_messages(cfg: &Config, f: &PageFacts) -> bool {
    static MSG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:response|res|resp|result|data|payload|r)\??\.(?:data\??\.)?message\b").unwrap()
    });
    if f.code_files.is_empty() {
        return true;
    }
    let root = cfg
        .frontends
        .iter()
        .find(|fe| Config::repo_name(fe) == f.page.app)
        .cloned()
        .unwrap_or_else(|| cfg.backend.clone());
    f.code_files
        .iter()
        .any(|c| std::fs::read_to_string(root.join(c)).is_ok_and(|t| MSG.is_match(&t)))
}

/// A component file as the app's router names it (`/src/pages/X.vue`, `/app/src/pages/X.vue`) → (app, path in it).
fn app_file(frontends: &[PathBuf], file: &str) -> Option<(String, String)> {
    let file = file.split('?').next().unwrap_or_default().trim_start_matches('/');
    let mut tries = vec![file.to_string()];
    let mut rest = file;
    while let Some(i) = rest.find("src/") {
        tries.push(rest[i..].to_string());
        rest = &rest[i + 4..];
    }
    for fe in frontends {
        for t in &tries {
            if fe.join(t).is_file() {
                return Some((Config::repo_name(fe), t.clone()));
            }
        }
    }
    None
}

/// Every .vue file of the frontends by component name (its file stem) → (app, path).
fn vue_files_by_name(frontends: &[PathBuf]) -> HashMap<String, (String, String)> {
    fn walk(dir: &Path, root: &Path, app: &str, out: &mut HashMap<String, (String, String)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if name != "node_modules" && !name.starts_with('.') && name != "dist" {
                    walk(&p, root, app, out);
                }
            } else if let Some(stem) = name.strip_suffix(".vue") {
                let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
                out.entry(stem.to_string()).or_insert((app.to_string(), rel));
            }
        }
    }
    let mut out = HashMap::new();
    for fe in frontends {
        walk(&fe.join("src"), fe, &Config::repo_name(fe), &mut out);
    }
    out
}

/// The running app's routes, from the cache while the route files are unchanged, else read again (doctest's own
/// tests only read the cache).
fn live_routes_cached(cfg: &Config) -> Option<Vec<Value>> {
    if cfg.app.base_url.is_empty() || !cfg.app.live_routes {
        return None;
    }
    let path = if cfg!(test) {
        cfg.out.join(".doctest/cache/routes.json")
    } else {
        let id = crate::hash::sha256(&cfg.root.to_string_lossy());
        crate::reports::tool_root()
            .join("cache")
            .join(&id[..12])
            .join("routes.json")
    };
    let key = routes_key(cfg);
    let cached: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(c) = &cached
        && c["key"] == key.as_str()
    {
        return c["routes"].as_array().cloned();
    }
    if cfg!(test) {
        return cached.and_then(|c| c["routes"].as_array().cloned());
    }
    match crate::shots::live_routes(cfg) {
        Ok(routes) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, serde_json::json!({"key": key, "routes": routes}).to_string());
            Some(routes)
        }
        // the app is not running: the last list read, if any
        Err(_) => cached.and_then(|c| c["routes"].as_array().cloned()),
    }
}

/// What the route list depends on: the app's address and the route files (name, size, time) of every frontend.
fn routes_key(cfg: &Config) -> String {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if p.is_dir() {
                if name != "node_modules" && !name.starts_with('.') && name != "dist" {
                    walk(&p, out);
                }
            } else if name.contains("route") || p.parent().is_some_and(|d| d.ends_with("router")) {
                let m = std::fs::metadata(&p).ok();
                let t = m
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                out.push(format!("{}:{}:{t}", p.display(), m.map(|m| m.len()).unwrap_or(0)));
            }
        }
    }
    let mut parts = vec![cfg.app.base_url.clone(), crate::update::LONG_VERSION.to_string()];
    for fe in &cfg.frontends {
        walk(&fe.join("src"), &mut parts);
    }
    parts.sort();
    crate::hash::sha256(&parts.join("\n"))
}

/// Boolean props a template passes to a child tag: `<X approval />` and `:approval="true"` are true,
/// `:approval="false"` false.
pub fn static_flags(parent: &str, tag: &str, known: &BTreeMap<String, bool>) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    if tag.is_empty() {
        return out;
    }
    let kebab: String = tag
        .chars()
        .enumerate()
        .flat_map(|(i, c)| {
            if c.is_ascii_uppercase() && i > 0 {
                vec!['-', c.to_ascii_lowercase()]
            } else {
                vec![c.to_ascii_lowercase()]
            }
        })
        .collect();
    let Ok(open) = Regex::new(&format!(
        r"<(?:{}|{})\b([^>]*)>",
        regex::escape(tag),
        regex::escape(&kebab)
    )) else {
        return out;
    };
    static BOUND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#":([\w-]+)\s*=\s*"(true|false)""#).unwrap());
    static ACTION_PROP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#":([\w-]*(?:new|create|add|delete|remove|export|import|print|download)[\w-]*)\s*=\s*"[^"]+""#)
            .unwrap()
    });
    static SET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#":([\w-]+)\s*=\s*"\s*['`\[{]"#).unwrap());
    static TERNARY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#":([\w-]+)\s*=\s*"\s*(!?)\s*(?:props\.)?(\w+)\s*\?\s*([^:"]+?)\s*:\s*([^"]+?)\s*""#).unwrap()
    });
    for c in open.captures_iter(parent) {
        let attrs = c[1].trim_end_matches('/');
        for b in BOUND.captures_iter(attrs) {
            out.insert(camel(&b[1]), &b[2] == "true");
        }
        // `:new-to="'/booking/new'"`, `:columns="[...]"`: set
        for b in SET.captures_iter(attrs) {
            out.entry(camel(&b[1])).or_insert(true);
        }
        // an action prop bound to anything (`:new-to="newRoute"`): taken as set unless a known flag says otherwise
        for b in ACTION_PROP.captures_iter(attrs) {
            out.entry(camel(&b[1])).or_insert(true);
        }
        // `:new-to="approval ? null : '/x'"` with approval known: set or empty
        for t in TERNARY.captures_iter(attrs) {
            let Some(&cond) = known.get(&t[3]) else { continue };
            let cond = if &t[2] == "!" { !cond } else { cond };
            let chosen = if cond { &t[4] } else { &t[5] };
            let empty = matches!(chosen.trim(), "null" | "undefined" | "false" | "''" | "\"\"" | "0");
            out.insert(camel(&t[1]), !empty);
        }
        // bare attributes (no value, outside any quoted value): `approval`, `compact`
        static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'"#).unwrap());
        let unquoted = QUOTED.replace_all(attrs, "\"\"").to_string();
        for token in unquoted.split_whitespace() {
            let name = token.trim_end_matches('/');
            if !name.is_empty()
                && !name.contains('=')
                && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && !name.starts_with("v-")
            {
                out.entry(camel(name)).or_insert(true);
            }
        }
    }
    out
}

fn camel(kebab: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in kebab.chars() {
        if c == '-' {
            up = true;
        } else if up {
            out.push(c.to_ascii_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// A component's text without the template elements its boolean props hide: `v-if="!approval"` when approval is
/// true, `v-if="approval"` (and its `v-else`) when it is false.
pub fn prune_template(vue: &str, flags: &BTreeMap<String, bool>, strings: &BTreeMap<String, String>) -> String {
    static EQ: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"<([a-zA-Z][\w.-]*)\b[^>]*\bv-(if|else-if)\s*=\s*"\s*(?:props\.)?(\w+)\s*(===|==|!==|!=)\s*'([^']*)'\s*""#).unwrap()
    });
    static VIF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"<([a-zA-Z][\w.-]*)\b[^>]*\bv-(if|else-if)\s*=\s*"\s*(!?)\s*(?:props\.)?(\w+)\s*""#).unwrap()
    });
    static ELSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<([a-zA-Z][\w.-]*)\b[^>]*\bv-else\b").unwrap());
    static DISABLED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"<([a-zA-Z][\w.-]*)\b[^>]*?(?:\sdisabled(?:\s|/|>|="(?:true|disabled)?")|\s:disabled="true")"#)
            .unwrap()
    });
    let mut text = vue.to_string();
    // a control that is always disabled is not one a reader can use
    for _ in 0..200 {
        let Some(c) = DISABLED.captures(&text) else { break };
        let start = c.get(0).map(|m| m.start()).unwrap_or(0);
        let end = element_end(&text, start, &c[1]);
        if end <= start {
            break;
        }
        text.replace_range(start..end, "");
    }
    // repeat: removing one element shifts the rest
    for _ in 0..200 {
        let mut edit: Option<(usize, usize, String)> = None;
        // (tag, start, shown) for every v-if this page's props decide
        let mut decided: Vec<(String, usize, bool)> = Vec::new();
        for c in VIF.captures_iter(&text) {
            if let Some(&value) = flags.get(&c[4]) {
                decided.push((
                    c[1].to_string(),
                    c.get(0).map(|m| m.start()).unwrap_or(0),
                    if &c[3] == "!" { !value } else { value },
                ));
            }
        }
        for c in EQ.captures_iter(&text) {
            if let Some(value) = strings.get(&c[3]) {
                let equal = *value == c[5];
                let shown = if c[4].starts_with('!') { !equal } else { equal };
                decided.push((c[1].to_string(), c.get(0).map(|m| m.start()).unwrap_or(0), shown));
            }
        }
        decided.sort_by_key(|d| d.1);
        for (tag, start, shown) in decided {
            let end = element_end(&text, start, &tag);
            let rest = &text[end..];
            let lead = rest.len() - rest.trim_start().len();
            let sibling = ELSE.captures(&rest[lead..]).map(|m| {
                let s2 = end + lead;
                (s2, element_end(&text, s2, &m[1]))
            });
            if !shown {
                // gone; its v-else (if any) is what shows, so it loses the v-else
                let tail = match sibling {
                    Some((s2, e2)) => {
                        let el = &text[s2..e2];
                        let gt = el.find('>').unwrap_or(el.len());
                        let open = if el[..gt].contains("v-else-if") {
                            el[..gt].replacen("v-else-if", "v-if", 1)
                        } else {
                            el[..gt].replacen(" v-else", "", 1)
                        };
                        format!("{open}{}", &el[gt..])
                    }
                    None => String::new(),
                };
                let to = sibling.map(|(_, e2)| e2).unwrap_or(end);
                edit = Some((start, to, tail));
                break;
            }
            // shown: its v-else is hidden
            if let Some((s2, e2)) = sibling {
                edit = Some((s2, e2, String::new()));
                break;
            }
        }
        match edit {
            Some((a, b, with)) if b > a => text.replace_range(a..b, &with),
            _ => break,
        }
    }
    text
}

/// Does a template render this tag (`<FooBar` or `<foo-bar`)?
pub fn renders_tag(template: &str, tag: &str) -> bool {
    if tag.is_empty() {
        return true;
    }
    let kebab = kebab_case(tag);
    [tag.to_string(), kebab]
        .iter()
        .any(|t| Regex::new(&format!(r"<{}[\s/>]", regex::escape(t))).is_ok_and(|r| r.is_match(template)))
}

fn kebab_case(tag: &str) -> String {
    tag.chars()
        .enumerate()
        .flat_map(|(i, c)| {
            if c.is_ascii_uppercase() && i > 0 {
                vec!['-', c.to_ascii_lowercase()]
            } else {
                vec![c.to_ascii_lowercase()]
            }
        })
        .collect()
}

/// String props a template passes to a child tag: `kind="x"`, `:kind="'x'"`, `:kind="kind"` (from the parent's own
/// string props) and `v-bind="$props"` (all of them).
pub fn static_strings(parent: &str, tag: &str, known: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(open) = Regex::new(&format!(
        r"<(?:{}|{})\b([^>]*)>",
        regex::escape(tag),
        regex::escape(&kebab_case(tag))
    )) else {
        return out;
    };
    static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(:?)([\w-]+)\s*=\s*"([^"]*)""#).unwrap());
    for c in open.captures_iter(parent) {
        let attrs = &c[1];
        if attrs.contains("v-bind=\"$props\"") || attrs.contains("v-bind=\"props\"") {
            out.extend(known.clone());
        }
        for a in ATTR.captures_iter(attrs) {
            let (bound, name, value) = (&a[1] == ":", camel(&a[2]), a[3].trim());
            if name.starts_with("v-") || a[2].starts_with("v-") {
                continue;
            }
            if !bound {
                out.insert(name, value.to_string());
            } else if let Some(lit) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
                out.insert(name, lit.to_string());
            } else if let Some(v) = known.get(value.trim_start_matches("props.")) {
                out.insert(name, v.clone());
            }
        }
    }
    out
}

/// The function or object method around a line of a module (`async generate(id) {`, `save: (x) =>`, `export
/// function load(`), found by indentation.
pub fn enclosing_member(text: &str, line: usize) -> Option<String> {
    static DEF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(\s*)(?:export\s+)?(?:(?:async\s+)?function\s+(\w+)|(?:const|let)\s+(\w+)\s*=\s*(?:async\s*)?(?:\([^)]*\)|\w+)\s*=>|(?:async\s+)?(\w+)\s*\([^)]*\)\s*(?::\s*[^={]+)?\{|(\w+)\s*:\s*(?:async\s*)?(?:function\b|\([^)]*\)\s*(?::\s*[^=]+)?=>|\w+\s*=>))").unwrap()
    });
    let lines: Vec<&str> = text.lines().collect();
    let at = line.checked_sub(1)?;
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut limit = lines.get(at).map(|l| indent(l))?;
    for l in lines[..=at.min(lines.len() - 1)].iter().rev() {
        if l.trim().is_empty() {
            continue;
        }
        if let Some(c) = DEF.captures(l) {
            let name = c
                .get(2)
                .or(c.get(3))
                .or(c.get(4))
                .or(c.get(5))
                .map(|m| m.as_str())
                .unwrap_or_default();
            if !matches!(
                name,
                "if" | "for" | "while" | "switch" | "catch" | "return" | "function"
            ) && indent(l) <= limit
            {
                return Some(name.to_string());
            }
        }
        limit = limit.min(indent(l));
    }
    None
}

/// Where the element opened at `start` ends: after `/>`, or after its matching `</tag>`.
fn element_end(text: &str, start: usize, tag: &str) -> usize {
    let Some(gt) = text[start..].find('>') else {
        return text.len();
    };
    let open_end = start + gt + 1;
    if text[..open_end].ends_with("/>") {
        return open_end;
    }
    let Ok(re) = Regex::new(&format!(r"<(/?){}\b[^>]*?(/?)>", regex::escape(tag))) else {
        return text.len();
    };
    let mut depth = 1;
    for m in re.captures_iter(&text[open_end..]) {
        let closing = &m[1] == "/";
        let self_closing = &m[2] == "/";
        if closing {
            depth -= 1;
            if depth == 0 {
                return open_end + m.get(0).map(|x| x.end()).unwrap_or(0);
            }
        } else if !self_closing {
            depth += 1;
        }
    }
    text.len()
}

/// Script code without the branches its string props rule out: `kind === 'other' ? a() : b()` keeps `b()`, and
/// `if (kind === 'other') { … }` goes.
pub fn prune_script(code: &str, props: &BTreeMap<String, String>) -> String {
    if props.is_empty() {
        return code.to_string();
    }
    static TERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:props\.)?(\w+)(?:\.value)?\s*===?\s*'([\w-]+)'\s*\?\s*([^:?]+?)\s*:").unwrap()
    });
    static IF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"if\s*\(\s*(?:props\.)?(\w+)(?:\.value)?\s*===?\s*'([\w-]+)'\s*\)\s*\{").unwrap());
    let mut out = TERN
        .replace_all(code, |c: &regex::Captures| match props.get(&c[1]) {
            Some(v) if *v != c[2] => String::new(),
            _ => c[0].to_string(),
        })
        .to_string();
    for _ in 0..100 {
        let Some(c) = IF
            .captures_iter(&out)
            .find(|c| props.get(&c[1]).is_some_and(|v| *v != c[2]))
        else {
            break;
        };
        let m = c.get(0).expect("match");
        let open = m.end() - 1;
        let mut depth = 0i32;
        let mut end = out.len();
        for (i, ch) in out[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.replace_range(m.start()..end, "");
    }
    out
}

/// The functions a composable defines, and those of them the page uses: named in its templates, or called by a used
/// one (transitively).
pub fn wired_functions(code: &str, templates: &str, scripts: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let lines: Vec<&str> = code.lines().collect();
    let mut bodies: BTreeMap<String, String> = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(m) = enclosing_member(code, i + 1) {
            let b = bodies.entry(m).or_default();
            b.push_str(line);
            b.push('\n');
        }
    }
    let defined: BTreeSet<String> = bodies.keys().cloned().collect();
    let word =
        |name: &str, text: &str| Regex::new(&format!(r"\b{}\b", regex::escape(name))).is_ok_and(|r| r.is_match(text));
    // called (`name(`) or handed over as a callback (`(name)`, `, name)`), not merely listed in a return value
    let called = |name: &str, text: &str| {
        Regex::new(&format!(
            r"\b{n}\s*\(|\(\s*{n}\s*\)|,\s*{n}\s*\)",
            n = regex::escape(name)
        ))
        .is_ok_and(|r| r.is_match(text))
    };
    let mut wired: BTreeSet<String> = defined
        .iter()
        .filter(|m| word(m, templates) || called(m, scripts))
        .cloned()
        .collect();
    // the composable's own top level (onMounted(load), watch(…, refresh)) runs on its own
    let top: String = (0..lines.len())
        .filter(|i| enclosing_member(code, i + 1).is_none())
        .map(|i| lines[i])
        .collect::<Vec<_>>()
        .join("\n");
    let top_calls: BTreeSet<String> = defined
        .iter()
        .filter(|m| Regex::new(&format!(r"\b{}\s*(?:\(|\)|,)", regex::escape(m))).is_ok_and(|r| r.is_match(&top)))
        .cloned()
        .collect();
    // functions handed out in the return value are used by whatever renders them only if a template names them
    wired.extend(top_calls.into_iter().filter(|m| !returned_only(m, &top)));
    loop {
        let mut more = Vec::new();
        for m in &wired {
            let body = bodies.get(m).map(String::as_str).unwrap_or("");
            for d in &defined {
                if !wired.contains(d) && d != m && called(d, body) {
                    more.push(d.clone());
                }
            }
        }
        if more.is_empty() {
            break;
        }
        wired.extend(more);
    }
    (defined, wired)
}

/// Is the name only listed in a `return { … }` (handed to the template), not called at the top level?
fn returned_only(name: &str, top: &str) -> bool {
    let call = Regex::new(&format!(r"\b{}\s*\(", regex::escape(name))).is_ok_and(|r| r.is_match(top));
    !call
}

fn line_no_of(at: &str) -> usize {
    at.rsplit(':').next().and_then(|l| l.parse().ok()).unwrap_or(0)
}

/// Does code call this function or method (`api.generate(`, `generate(`, or pass it on: `generate,`)?
pub fn mentions_call(code: &str, name: &str) -> bool {
    Regex::new(&format!(r"(?:\.|\b){}\s*(?:\(|,|\}}|\))", regex::escape(name))).is_ok_and(|r| r.is_match(code))
}

/// A URL that starts with a run-time value: `` `${base}/x` `` or `base + '/x'` as a call's first argument.
static DYNAMIC_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:\(|,)\s*(?:`\$\{|[A-Za-z_][\w.\]\[']*\s*\+\s*['"`])"#).unwrap());

/// Kinds a page's code passes as a call's first argument: `api.downloadReport('asset-fsr', …)` → asset-fsr.
pub fn call_kinds(code: &str) -> Vec<String> {
    static K: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\b\w+\.\w+\(\s*['"]([a-z][a-z0-9]*(?:-[a-z0-9]+)*)['"]"#).unwrap());
    let mut out: Vec<String> = K.captures_iter(code).map(|c| c[1].to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// Words of a page key that say what kind of screen it is, not what it shows.
const GENERIC_KEY_WORDS: [&str; 14] = [
    "admin",
    "selfcare",
    "contractor",
    "view",
    "listing",
    "list",
    "detail",
    "details",
    "index",
    "create",
    "edit",
    "new",
    "show",
    "form",
];

/// The innermost object literal that holds the string `needle` (`detailRoute: 'admin.x.view'` inside a contract).
pub fn block_naming<'a>(text: &'a str, needle: &str) -> Option<&'a str> {
    let at = ["'", "\"", "`"]
        .iter()
        .find_map(|q| text.find(&format!("{q}{needle}{q}")))?;
    let mut depth = 0i32;
    let mut open = None;
    for (i, c) in text[..at].char_indices().rev() {
        match c {
            '}' => depth += 1,
            '{' => {
                if depth == 0 {
                    open = Some(i);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let start = open?;
    let mut depth = 0i32;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The fields of an object that hold a path: `endpoint: 'admin/eform/delivery'` → endpoint → admin/eform/delivery.
pub fn path_fields(region: &str) -> BTreeMap<String, String> {
    static F: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\b(\w+)\s*:\s*['"`]([a-zA-Z][\w\-]*(?:/[\w\-{}]+)+)/?['"`]"#).unwrap());
    F.captures_iter(region)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// Every value of these fields that holds a path, anywhere in a file: (field, path).
pub fn path_values_named(text: &str, names: &[String]) -> Vec<(String, String)> {
    path_fields_all(text)
        .into_iter()
        .filter(|(n, _)| names.contains(n))
        .collect()
}

fn path_fields_all(text: &str) -> Vec<(String, String)> {
    static F: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\b(\w+)\s*:\s*['"`]([a-zA-Z][\w\-]*(?:/[\w\-{}]+)+)/?['"`]"#).unwrap());
    F.captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// Template paths that start with one of these fields (`${contract.endpoint}/${id}/approve`), joined to its value.
pub fn joined_paths(text: &str, fields: &BTreeMap<String, String>) -> Vec<String> {
    static T: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`\$\{([^}]+)\}([^`]*)`").unwrap());
    static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{[^}]*\}").unwrap());
    let mut out = Vec::new();
    for c in T.captures_iter(text) {
        let name = c[1]
            .trim()
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(['?', ')']);
        let Some(base) = fields.get(name) else { continue };
        let tail = VAR.replace_all(&c[2], "{p}");
        let path = format!("{base}{}", tail.split('?').next().unwrap_or_default())
            .trim_matches('/')
            .to_string();
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Path values a module keeps under a kind in a flat map: `department: 'admin/general/parameter/department'`.
pub fn keyed_values(text: &str, key: &str) -> Vec<String> {
    let Ok(re) = Regex::new(&format!(
        r#"(?:['"`]{k}['"`]|(?:^|[\s{{,]){k})\s*:\s*['"`]([a-zA-Z][\w\-]*(?:/[\w\-{{}}]+)+)/?['"`]"#,
        k = regex::escape(key)
    )) else {
        return Vec::new();
    };
    let mut out: Vec<String> = re.captures_iter(text).map(|c| c[1].to_string()).collect();
    out.dedup();
    out
}

/// Template paths that start with a map lookup (`${endpoints[props.kind]}/${id}`, or a variable set from one),
/// joined to the page kind's base paths.
pub fn indexed_paths(text: &str, bases: &[String]) -> Vec<String> {
    static T: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`\$\{([^}]+)\}([^`]*)`").unwrap());
    static FROM_MAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\w+)\s*=\s*[\w.]+\s*\[[^\]]+\]").unwrap());
    static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{[^}]*\}").unwrap());
    if bases.is_empty() {
        return Vec::new();
    }
    let from_map: BTreeSet<String> = FROM_MAP.captures_iter(text).map(|c| c[1].to_string()).collect();
    let mut out = Vec::new();
    for c in T.captures_iter(text) {
        let expr = c[1].trim();
        if !(expr.contains('[') || from_map.contains(last_ident(expr))) {
            continue;
        }
        let tail = VAR.replace_all(&c[2], "{p}");
        for base in bases {
            let path = format!("{base}{}", tail.split('?').next().unwrap_or_default())
                .trim_matches('/')
                .to_string();
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// The object a module keeps for one kind: the `{…}` after `'kind':`, `"kind":`, `kind:` or `['kind']:`.
pub fn keyed_block<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let re = Regex::new(&format!(
        r#"(?:['"`]{k}['"`]|\b{k}\b|\[['"`]{k}['"`]\])\s*:\s*\{{"#,
        k = regex::escape(key)
    ))
    .ok()?;
    let m = re.find(text)?;
    let start = m.end() - 1;
    let mut depth = 0i32;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// API paths written as strings in `region` (`'admin/eform/delivery'`, `` `${base}/${id}/approve` ``), with
/// `${a.b}` filled from `b: '…'` or `const b = '…'` (in the region first, then the whole file); a part that cannot be
/// filled becomes a parameter.
pub fn literal_paths(region: &str, file: &str, kinds: &[String]) -> Vec<String> {
    static STR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"'([^'\n]*)'|"([^"\n]*)"|`([^`]*)`"#).unwrap());
    static ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\b(\w+)\s*(?::|=)\s*(?:'([^'\n]*/[^'\n]*)'|"([^"\n]*/[^"\n]*)"|`([^`$]*/[^`$]*)`)"#).unwrap()
    });
    static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([^}]*)\}").unwrap());
    let mut values: HashMap<String, String> = HashMap::new();
    for t in [file, region] {
        for c in ASSIGN.captures_iter(t) {
            let v = c
                .get(2)
                .or(c.get(3))
                .or(c.get(4))
                .map(|m| m.as_str())
                .unwrap_or_default();
            values.insert(c[1].to_string(), v.to_string());
        }
        // `const base = (id) => `admin/x/${id}/y``: a function that builds a base path
        for c in FN.captures_iter(t) {
            values.insert(c[1].to_string(), c[2].to_string());
        }
    }
    static FN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:const|let)\s+(\w+)\s*=\s*\([^)]*\)\s*=>\s*`([^`]*/[^`]*)`").unwrap());
    // `…/fsr${editing ? `/${id}` : ''}`: the address both without and with the optional part
    static OPT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\$\{[^{}`]*\?\s*`([^`]*)`\s*:\s*(?:''|""|``)\s*\}"#).unwrap());
    let expanded;
    let region = if OPT.is_match(region) {
        let without = OPT.replace_all(region, "").to_string();
        let with = OPT
            .replace_all(region, |c: &regex::Captures| c[1].to_string())
            .to_string();
        expanded = format!("{without}\n{with}");
        expanded.as_str()
    } else {
        region
    };
    // only strings used as an address: an HTTP call's first argument or a url/endpoint/base field (not ACL names,
    // menu paths or route paths that only look like one)
    static USE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:\.(?:get|post|put|patch|delete|request|head)\s*(?:<[^>]*>)?\s*\(\s*|\bfetch\s*\(\s*|\b\w*(?:url|uri|endpoint|base|api)\w*\s*[:=]\s*)$").unwrap()
    });
    let mut out = Vec::new();
    for c in STR.captures_iter(region) {
        let raw = c
            .get(1)
            .or(c.get(2))
            .or(c.get(3))
            .map(|m| m.as_str())
            .unwrap_or_default();
        if !raw.contains('/')
            || raw.contains(' ')
            || raw.contains("://")
            || raw.starts_with('.')
            || raw.starts_with('@')
        {
            continue;
        }
        let before = &region[..c.get(0).map(|m| m.start()).unwrap_or(0)];
        let mut from = before.len().saturating_sub(60);
        while !before.is_char_boundary(from) {
            from += 1;
        }
        let before = &before[from..];
        if !USE.is_match(before) {
            continue;
        }
        // `${props.kind}` stands for the page's kind (from its key); one path per kind
        let by_kind = VAR
            .captures_iter(raw)
            .any(|v| matches!(last_ident(&v[1]), "kind" | "type" | "entity" | "resource"));
        let fills: Vec<Option<&String>> = if by_kind && !kinds.is_empty() {
            kinds.iter().map(Some).collect()
        } else {
            vec![None]
        };
        for kind in fills {
            let filled = VAR.replace_all(raw, |v: &regex::Captures| {
                let name = last_ident(&v[1]);
                if let (Some(k), true) = (kind, matches!(name, "kind" | "type" | "entity" | "resource")) {
                    return k.clone();
                }
                values.get(name).cloned().unwrap_or_else(|| "{p}".to_string())
            });
            // a value that is itself a template (`base(id)`): its own parts become parameters
            let filled = VAR.replace_all(&filled, "{p}");
            let path = filled
                .split('?')
                .next()
                .unwrap_or_default()
                .trim_matches('/')
                .to_string();
            // a path that starts with an unknown part cannot be placed
            if path.is_empty() || path.starts_with('{') || path.ends_with(".vue") || path.contains(".ts") {
                continue;
            }
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// `props.contract.endpoint` → `endpoint`.
fn last_ident(expr: &str) -> &str {
    // `base(assetId)` → base
    let expr = expr.trim();
    expr.split('(')
        .next()
        .unwrap_or(expr)
        .rsplit(['.', ' '])
        .next()
        .unwrap_or_default()
        .trim_end_matches(['?', ')'])
}

/// The API paths written inside the functions a page imports by name from a module.
pub fn literal_paths_in(text: &str, functions: &BTreeSet<String>, kinds: &[String], own_code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen_fn = BTreeSet::new();
    for (i, _) in text.lines().enumerate() {
        if let Some(name) = enclosing_export(text, i + 1)
            && functions.contains(&name)
        {
            seen_fn.insert(name);
        }
    }
    // each function's lines, read as one region; of an exported object, only the methods the page calls
    let lines: Vec<&str> = text.lines().collect();
    let mut region = String::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(export) = enclosing_export(text, i + 1).filter(|n| seen_fn.contains(n)) else {
            continue;
        };
        let used = match enclosing_member(text, i + 1).filter(|m| *m != export) {
            Some(member) => Regex::new(&format!(
                r"\b{}\s*\??\.\s*{}\b",
                regex::escape(&export),
                regex::escape(&member)
            ))
            .is_ok_and(|r| r.is_match(own_code)),
            None => true,
        };
        if used {
            region.push_str(l);
            region.push('\n');
        }
    }
    for p in literal_paths(&region, text, kinds) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Does a path written in the frontend name this route? `api/` in front of either is ignored; `{…}` on either side
/// matches any one part.
pub fn path_matches(lit: &str, uri: &str) -> bool {
    let strip = |p: &str| -> Vec<String> {
        let p = p.trim_matches('/');
        let p = p.strip_prefix("api/").unwrap_or(p);
        p.split('/').map(String::from).collect()
    };
    let (a, b) = (strip(lit), strip(uri));
    a.len() == b.len()
        && a.len() >= 2
        && a.iter()
            .zip(&b)
            .all(|(x, y)| x == y || x.starts_with('{') || y.starts_with('{'))
        && a.iter().zip(&b).filter(|(x, y)| x == y).count() >= 2
}

/// The `<template>` part of a component (the whole text when it has none).
pub fn template_part(vue: &str) -> &str {
    match (vue.find("<template"), vue.rfind("</template>")) {
        (Some(a), Some(b)) if b > a => &vue[a..b],
        _ => vue,
    }
}

/// Quoted strings that look like i18n keys (`'message.serviceType'`, `"labels.code"`): a dotted or camelCase word.
pub fn quoted_keys(text: &str) -> Vec<String> {
    static Q: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"['"`]([A-Za-z][A-Za-z0-9_-]*(?:\.[A-Za-z0-9_-]+)+|[a-z]+[A-Z][A-Za-z0-9]*)['"`]"#).unwrap()
    });
    let mut out: Vec<String> = Q.captures_iter(text).map(|c| c[1].to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// A module name without case and punctuation: `E-Form`, `eform` and `e_form` are one module.
pub fn module_key(module: &str) -> String {
    module
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// A route path with its parameter names left out (`/floor-plan/:floorId` and `/floor-plan/:id` are one URL).
pub fn same_path(path: &str) -> String {
    static PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":\w+\??|\{\w+\??\}").unwrap());
    PARAM.replace_all(path.trim_end_matches('/'), ":p").to_string()
}

/// A route container: its template is only a place for child pages (`<router-view/>`, maybe inside a transition or a
/// plain wrapper), with no text and no other component.
pub fn is_container(vue: &str) -> bool {
    static TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<([A-Z][A-Za-z0-9]*|[a-z][a-z0-9]*-[a-z0-9-]+)[\s/>]").unwrap());
    const HOLDERS: [&str; 8] = [
        "router-view",
        "RouterView",
        "nuxt-page",
        "NuxtPage",
        "transition",
        "Transition",
        "keep-alive",
        "KeepAlive",
    ];
    let Some(start) = vue.find("<template") else {
        return false;
    };
    let end = vue.rfind("</template>").unwrap_or(vue.len());
    let template = &vue[start..end];
    is_layout(template)
        && template_literals(vue).is_empty()
        && TAG
            .captures_iter(template)
            .all(|c| HOLDERS.contains(&&c[1]) || c[1].eq_ignore_ascii_case("component"))
}

/// A layout: a component that renders the routed page inside it.
pub fn is_layout(vue: &str) -> bool {
    vue.contains("<router-view")
        || vue.contains("<RouterView")
        || vue.contains("<nuxt-page")
        || vue.contains("<NuxtPage")
}

/// A screen many routes share (route props such as `{kind}` pick what it shows), or a component used all over.
pub fn is_shared(pages_using: usize, used_by: usize) -> bool {
    pages_using >= 2 || used_by >= 2
}

/// Button words that show an action is offered on the page.
fn button_words(action: &str) -> &'static [&'static str] {
    match action {
        "Add" => &["new", "add", "create", "save", "submit"],
        "Edit" => &["edit", "update", "save"],
        "Delete" => &["delete", "remove", "trash"],
        "Export" => &["export", "excel", "xls", "pdf", "download", "print"],
        "Import" => &["import", "upload"],
        "Approve" => &["approve"],
        "Reject" => &["reject"],
        "Submit" => &["submit", "send"],
        "Cancel" => &["cancel"],
        "Void" => &["void"],
        "Generate" => &["generate"],
        "Send" => &["send", "email"],
        "Restore" => &["restore"],
        "Duplicate" | "Copy" => &["duplicate", "copy", "clone"],
        "Terminate" => &["terminate"],
        "Renew" => &["renew"],
        "Publish" => &["publish"],
        "Post" => &["post"],
        "Upload" => &["upload"],
        "Download" | "Print" => &["download", "print"],
        _ => &[],
    }
}

/// The actions a guide must cover (and show in screenshots): the page's own calls whose control is on screen.
pub fn required_actions(f: &PageFacts) -> Vec<Action> {
    actions(f)
        .into_iter()
        .filter(|a| !a.possible && shows_button(f, &a.name))
        .collect()
}

/// Does the page show a control for this action (its words on screen)?
pub fn shows_control(f: &PageFacts, action: &str) -> bool {
    shows_button(f, action)
}

fn shows_button(f: &PageFacts, action: &str) -> bool {
    let words = button_words(action);
    let texts: Box<dyn Iterator<Item = &String>> = if f.screen_texts.is_empty() {
        Box::new(f.labels.values().flatten().chain(&f.literals))
    } else {
        Box::new(f.screen_texts.iter())
    };
    texts.into_iter().any(|t| {
        let lower = t.to_lowercase();
        lower.split(|c: char| !c.is_alphanumeric()).any(|w| words.contains(&w))
    })
}

fn action_of(e: &Endpoint, name: String) -> Action {
    let (success, errors): (Vec<String>, Vec<String>) = e.messages.iter().cloned().partition(|m| reads_as_success(m));
    Action {
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
        possible: e.possible,
    }
}

/// Actions the page may offer (reached only through shared code, no matching button on screen): document one only
/// when the page shows it.
pub fn possible_actions(f: &PageFacts) -> Vec<Action> {
    let certain: Vec<String> = actions(f).into_iter().map(|a| a.name).collect();
    let mut out: Vec<Action> = Vec::new();
    for e in f.endpoints.iter().filter(|e| e.possible) {
        let Some(name) = page_action_name(f, e) else { continue };
        if certain.contains(&name) || out.iter().any(|a| a.name == name) {
            continue;
        }
        out.push(action_of(e, name));
    }
    out
}

/// An action's name on this page: a create call on a page that offers no Add/New/Create (a settings page that only
/// saves, its controller upserting) is the page's Edit.
fn page_action_name(f: &PageFacts, e: &Endpoint) -> Option<String> {
    let name = action_name(e)?;
    if name == "Add" {
        let page = format!("{} {}", f.page.key, f.page.path).to_lowercase();
        let creates = shows_words(f, &["new", "add", "create"])
            || ["create", "new", "add"].iter().any(|w| page.contains(w))
            || e.messages.iter().any(|m| {
                let m = m.to_lowercase();
                m.contains("created") || m.contains("added")
            });
        if !creates && shows_words(f, &["save", "update", "submit"]) {
            return Some("Edit".into());
        }
    }
    Some(name)
}

fn shows_words(f: &PageFacts, words: &[&str]) -> bool {
    let texts: Box<dyn Iterator<Item = &String>> = if f.screen_texts.is_empty() {
        Box::new(f.labels.values().flatten().chain(&f.literals))
    } else {
        Box::new(f.screen_texts.iter())
    };
    texts.into_iter().any(|t| {
        t.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| words.contains(&w))
    })
}

/// The page's actions, one per name, in the order of its endpoints: calls its own components make, and calls from
/// shared code when the page shows a matching button.
pub fn actions(f: &PageFacts) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    for e in &f.endpoints {
        let Some(name) = page_action_name(f, e) else { continue };
        if e.possible && !shows_button(f, &name) {
            continue;
        }
        if out.iter().any(|a| a.name == name) {
            continue;
        }
        out.push(action_of(e, name));
    }
    out
}

/// Does the action work on an existing record (so it cannot be shown on a page with no data)?
fn needs_a_record(a: &Action) -> bool {
    !matches!(
        a.name.as_str(),
        "Add" | "Import" | "Upload" | "Export" | "Generate" | "Download" | "Print"
    )
}

/// The screenshots a page guide must have: the page, one form shot that covers Add and Edit, and one per other
/// action. With `no_data` (the page has no records to show), only the page and what works without a record.
pub fn screenshot_plan_for(actions: &[Action], no_data: bool) -> Vec<String> {
    let mut out = vec!["the page as it opens (list or form, with demo data if there is any)".to_string()];
    let form: Vec<&str> = actions
        .iter()
        .filter(|a| a.name == "Add" || a.name == "Edit")
        .filter(|a| !no_data || !needs_a_record(a))
        .map(|a| a.name.as_str())
        .collect();
    if !form.is_empty() {
        out.push(format!(
            "{}: the form (one shot covers {}; filled in, before saving)",
            form.join("/"),
            if form.len() > 1 { "both" } else { "it" }
        ));
    }
    for a in actions {
        if a.name == "Add" || a.name == "Edit" || (no_data && needs_a_record(a)) {
            continue;
        }
        out.push(match a.name.as_str() {
            "Delete" => "Delete: the confirm dialog".to_string(),
            other => format!("{other}: the button or dialog"),
        });
    }
    out
}

/// The screenshots a page guide must have (see [`screenshot_plan_for`]).
pub fn screenshot_plan(actions: &[Action]) -> Vec<String> {
    screenshot_plan_for(actions, false)
}

/// Screenshots worth adding when they make a step clearer; never required.
pub fn optional_shots(actions: &[Action]) -> Vec<String> {
    let mut out = Vec::new();
    for a in actions {
        match a.name.as_str() {
            "Add" => {
                out.push("Add: the result after saving (the message and the new row)".to_string());
                if !a.required.is_empty() {
                    out.push(format!(
                        "Add: a validation error (save with {} empty)",
                        a.required.join(", ")
                    ));
                }
            }
            "Delete" => out.push("Delete: the list after deleting".into()),
            other => out.push(format!("{other}: the result")),
        }
    }
    out
}

/// A guide that says its page has no data to show (`<!-- no-data -->`, optionally with a reason).
pub fn says_no_data(body: &str) -> bool {
    body.contains("<!-- no-data")
}

/// `member.name` / `banner_type` → `Member name` / `Banner type` (how Filament and most UIs label a field).
pub fn headline(field: &str) -> String {
    let words: Vec<String> = field
        .split(['.', '_', '-'])
        .filter(|w| !w.is_empty())
        .flat_map(|w| {
            // camelCase → camel case
            let mut parts = vec![String::new()];
            for ch in w.chars() {
                if ch.is_uppercase() && !parts.last().is_some_and(|p| p.is_empty()) {
                    parts.push(String::new());
                }
                parts.last_mut().expect("one part").push(ch.to_ascii_lowercase());
            }
            parts
        })
        .collect();
    let s = words.join(" ");
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// A router's catch-all ("not found") route, not a screen to document.
pub fn is_catch_all(path: &str) -> bool {
    path == "*" || path == "/*" || path.contains("(.*)") || path.contains(":pathMatch")
}

/// The Blade view a route renders: its `view` field, or `view('name'` in what codemap says it returns.
fn view_of(route: &Value) -> Option<String> {
    if let Some(v) = route["view"].as_str().filter(|v| !v.is_empty()) {
        return Some(v.to_string());
    }
    static VIEW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"view\(\s*['"]([\w.\-/:]+)['"]"#).unwrap());
    VIEW.captures(route["returns"].as_str().unwrap_or_default())
        .map(|c| c[1].to_string())
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

static SCRIPT_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b(?:label|title|text|header|headerName|placeholder|caption|tooltip|buttonText|confirmText|confirmButtonText|cancelButtonText|emptyText)\s*:\s*(?:'([^'\n]*)'|"([^"\n]*)")"#).unwrap()
});

/// Screen text set in a component's script: column headers, button and dialog texts (`label: 'Name'`,
/// `title: "Delete booking?"`). i18n keys and paths are left out.
pub fn script_literals(vue: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in SCRIPT_TEXT.captures_iter(vue) {
        let s = c.get(1).or(c.get(2)).map(|m| m.as_str().trim()).unwrap_or_default();
        let looks_like_key = !s.contains(' ') && (s.contains('.') || s.contains('/') || s.contains('_'));
        let lower_word = !s.contains(' ') && s.chars().next().is_some_and(|ch| ch.is_lowercase());
        if s.chars().any(char::is_alphabetic) && !looks_like_key && !lower_word && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    }
    out
}

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
/// Prefix of the current fingerprint format; fingerprints without it were made by an older doctest. Bump it whenever
/// doctest reads pages differently (new kinds of screen text, better endpoint attribution), so guides that were up
/// to date carry over instead of all turning stale.
pub const FINGERPRINT_VERSION: &str = "v6:";

/// What a page's guide depends on: what the user sees (path, labels, literal texts, menu path) and what its actions
/// do (endpoints with their rules, permissions and messages). doctest's own bookkeeping (code files, components,
/// where an action is defined) is left out, so a doctest upgrade does not make every guide stale.
pub fn fingerprint(f: &PageFacts, labels_hash: &str) -> String {
    let mut labels = f.labels.clone();
    for v in labels.values_mut() {
        v.sort();
        v.dedup();
    }
    let mut literals = f.literals.clone();
    literals.sort();
    literals.dedup();
    let mut menu = f.menu_titles.clone();
    menu.sort();
    let mut endpoints: Vec<serde_json::Value> = f
        .endpoints
        .iter()
        .map(|e| {
            let mut perms = e.permissions.clone();
            perms.sort();
            let mut msgs = e.messages.clone();
            msgs.sort();
            serde_json::json!([e.method, e.uri, e.rules, perms, msgs])
        })
        .collect();
    endpoints.sort_by_key(|v| v.to_string());
    endpoints.dedup();
    let text = serde_json::json!({
        "path": f.page.path,
        "portal": f.page.portal,
        "labels": labels,
        "literals": literals,
        "menu": menu,
        "endpoints": endpoints,
    })
    .to_string();
    format!(
        "{FINGERPRINT_VERSION}{}",
        crate::hash::sha256(&format!("{text}\n{labels_hash}"))
    )
}

/// A guide checked with an older fingerprint format, up to date when it was last planned: it stays up to date.
pub fn carries_over(documented: Option<&str>, last_fingerprint: &str) -> bool {
    documented.is_some_and(|d| !d.starts_with(FINGERPRINT_VERSION) && d == last_fingerprint)
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
    fn routes_the_running_app_registers_are_pages_too() {
        let (_d, mut cfg, mut api, labels) = setup();
        cfg.app.base_url = "http://127.0.0.1:1".into();
        let web = cfg.frontends[0].clone();
        std::fs::create_dir_all(web.join("src/pages/params")).unwrap();
        std::fs::write(
            web.join("src/pages/params/ParameterList.vue"),
            "<template><h1>Parameters</h1></template>",
        )
        .unwrap();
        // what the router answered last time (doctest's tests read only the cache)
        let routes = serde_json::json!([
            {"name": "shop.orders.create", "path": "/shop/orders/create", "file": "/src/pages/ShopOrderForm.vue", "props": {"kind": "order"}},
            {"name": "shop.parameter.colour", "path": "/shop/parameter/colour", "file": null, "component": "ParameterList", "props": {"kind": "colour"}},
            {"name": "shop.parameter.size", "path": "/shop/parameter/size", "file": "/app/src/pages/params/ParameterList.vue", "props": {"kind": "size"}},
            {"name": "shop.old", "path": "/shop/old", "redirect": true, "file": "/src/pages/ShopOrderForm.vue"},
            {"name": "shop.gone", "path": "/shop/gone", "file": "/src/pages/Missing.vue"}
        ]);
        let cache = cfg.out.join(".doctest/cache/routes.json");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, serde_json::json!({"key": "old", "routes": routes}).to_string()).unwrap();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let pages = fb.pages().unwrap();
        let keys: Vec<&str> = pages.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys.len(), 3, "{keys:?}");
        let colour = pages.iter().find(|p| p.key == "shop.parameter.colour").unwrap();
        assert_eq!(colour.component, PathBuf::from("src/pages/params/ParameterList.vue"));
        assert_eq!(colour.props["kind"], "colour");
        let size = pages.iter().find(|p| p.key == "shop.parameter.size").unwrap();
        assert_eq!(size.component, PathBuf::from("src/pages/params/ParameterList.vue"));
        assert_eq!(
            pages.iter().find(|p| p.key == "shop.orders.create").unwrap().props["kind"],
            "order"
        );
        // switched off
        cfg.app.live_routes = false;
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        assert_eq!(fb.pages().unwrap().len(), 1);
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

    fn cms() -> (tempfile::TempDir, Config, FixtureCodemap, Labels) {
        let d = tempfile::tempdir().unwrap();
        let cms = fixtures().join("cms");
        std::fs::write(
            d.path().join(".doctest.toml"),
            format!("[project]\nname=\"cms\"\nbackend=\"{}\"\n", cms.display()),
        )
        .unwrap();
        let cfg = Config::find(d.path()).unwrap();
        let labels = Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
        (d, cfg, FixtureCodemap::new(fixtures().join("codemap-cms")), labels)
    }

    #[test]
    fn filament_and_blade_screens_are_pages() {
        let (_d, cfg, mut api, labels) = cms();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let pages = fb.pages().unwrap();
        let by = |k: &str| {
            pages
                .iter()
                .find(|p| p.key == k)
                .unwrap_or_else(|| panic!("{k} not in {:?}", pages.iter().map(|p| &p.key).collect::<Vec<_>>()))
        };
        let f = by("filament.admin.resources.ad-orders.index");
        assert_eq!(
            (f.stack.as_str(), f.module.as_str(), f.portal.as_str()),
            ("filament", "ad-orders", "admin")
        );
        let b = by("portal.dashboard");
        assert_eq!(
            (b.stack.as_str(), b.path.as_str(), b.portal.as_str()),
            ("blade", "/portal/dashboard", "portal")
        );
        assert_eq!(b.component, PathBuf::from("resources/views/portal/dashboard.blade.php"));
        assert!(
            !pages.iter().any(|p| p.path.starts_with("/api")),
            "API routes are not screens"
        );
        assert_eq!(
            fb.portals.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["admin", "portal"]
        );
    }

    #[test]
    fn catch_all_routes_are_not_screens() {
        assert!(is_catch_all("/:pathMatch(.*)*"));
        assert!(is_catch_all("/:catchAll(.*)"));
        assert!(is_catch_all("*"));
        assert!(!is_catch_all("/admin/rems/data/lease-out/:id"));
    }

    #[test]
    fn filament_facts() {
        let (_d, cfg, mut api, labels) = cms();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let page = fb.pages().unwrap().into_iter().find(|p| p.stack == "filament").unwrap();
        let f = fb.facts(&page).unwrap();
        for want in ["Banner type", "Starts on date", "Amount", "Member name"] {
            assert!(
                f.labels["en"].iter().chain(&f.literals).any(|l| l == want),
                "{want} not in {:?} / {:?}",
                f.labels,
                f.literals
            );
        }
    }

    #[test]
    fn blade_facts() {
        let (_d, cfg, mut api, labels) = cms();
        let mut fb = FactBuilder::new(&mut api, &cfg, &labels);
        let page = fb
            .pages()
            .unwrap()
            .into_iter()
            .find(|p| p.key == "portal.dashboard")
            .unwrap();
        let f = fb.facts(&page).unwrap();
        assert!(
            f.literals.contains(&"Welcome back".to_string()) && f.literals.contains(&"Save profile".to_string()),
            "{:?}",
            f.literals
        );
        assert!(
            f.labels["en"].contains(&"Dashboard".to_string()) && f.labels["en"].contains(&"Phone number".to_string()),
            "{:?}",
            f.labels
        );
        assert!(
            f.endpoints
                .iter()
                .any(|e| e.method == "POST" && e.uri == "portal/profile"),
            "{:?}",
            f.endpoints
        );
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
    fn action_names_read_the_path_and_message() {
        let ep = |m: &str, uri: &str, msgs: &[&str]| Endpoint {
            method: m.into(),
            uri: uri.into(),
            messages: msgs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(
            action_name(&ep("POST", "api/ar/delete-receipt/{receiptId}", &[])).as_deref(),
            Some("Delete")
        );
        assert_eq!(
            action_name(&ep("POST", "api/ar/receipt/{id}", &["Record successfully deleted."])).as_deref(),
            Some("Delete")
        );
        assert_eq!(
            action_name(&ep("GET", "api/site-post-evaluation/{id}", &[])),
            None,
            "a GET does not post"
        );
        assert_eq!(
            action_name(&ep("POST", "api/site-post-evaluation/{id}", &[])).as_deref(),
            Some("Edit")
        );
        assert_eq!(
            action_name(&ep("POST", "api/journal/{id}/post", &[])).as_deref(),
            Some("Post")
        );
        assert_eq!(
            action_name(&ep("GET", "api/orders/export", &[])).as_deref(),
            Some("Export")
        );
        // a settings page that only saves: its create call is the page's Edit
        let mut f: PageFacts = serde_json::from_value(serde_json::json!({
            "page": {"key": "admin.system.configuration", "app": "web", "path": "/admin/system/configuration", "route_name": "c", "component": "C.vue", "module": "system"},
            "labels": {"en": ["Save", "Configuration"]}, "literals": [], "components": [], "code_files": [], "endpoints": [], "menu_titles": []
        }))
        .unwrap();
        f.endpoints = vec![ep("POST", "api/system/configuration", &[])];
        assert_eq!(actions(&f)[0].name, "Edit");
        f.labels
            .insert("en".into(), ["New", "Save"].iter().map(|s| s.to_string()).collect());
        assert_eq!(actions(&f)[0].name, "Add");
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
        // the page, one form for Add and Edit, Delete, Approve, Export
        assert_eq!(plan.len(), 5, "{plan:?}");
        assert!(plan[1].contains("Add/Edit") && plan[1].contains("both"), "{plan:?}");
        assert!(
            plan.iter().any(|s| s.contains("Delete") && s.contains("confirm")),
            "{plan:?}"
        );
        assert!(optional_shots(&a).iter().any(|s| s.contains("validation error")));
        // no data: the page, the Add form and Export (needs no record)
        let empty = screenshot_plan_for(&a, true);
        assert_eq!(empty.len(), 3, "{empty:?}");
        assert!(empty[1].starts_with("Add:"), "{empty:?}");
        assert!(says_no_data("x\n<!-- no-data: no bookings yet -->\n"));
    }

    #[test]
    fn script_texts_count_as_screen_text() {
        let vue = r#"<script>
const columns = [{ label: 'Name', field: 'name' }, { label: "Code" }, { title: 'Delete booking?' }];
const x = { label: 'booking.title', text: 'msg_key', header: 'id', placeholder: 'Search by name' };
</script>"#;
        assert_eq!(
            script_literals(vue),
            vec!["Name", "Code", "Delete booking?", "Search by name"]
        );
    }

    #[test]
    fn fingerprint_ignores_bookkeeping() {
        let f = sample();
        let fp = fingerprint(&f, "l");
        assert!(fp.starts_with(FINGERPRINT_VERSION));
        let mut g = f.clone();
        g.code_files.push("src/x.js".into());
        g.components.push("Extra".into());
        if let Some(e) = g.endpoints.first_mut() {
            e.at = Some("app/X.php:9".into());
            e.middleware.push("auth".into());
        }
        if let Some(e) = g.endpoints.last_mut() {
            e.possible = !e.possible;
        }
        assert_eq!(fingerprint(&g, "l"), fp, "internal fields changed the fingerprint");
        g.literals.push("New button".into());
        assert_ne!(fingerprint(&g, "l"), fp);
        assert!(carries_over(Some("abc"), "abc"));
        assert!(!carries_over(Some("abc"), "def"));
        assert!(!carries_over(Some(&fp), &fp));
        assert!(!carries_over(None, "abc"));
    }

    #[test]
    fn only_strings_used_as_addresses_are_paths() {
        let t = "const aros = { a: 'rems_admin/members' };\nconst menu = ['admin/rems/selfcare/newsletter'];\napi.get('admin/orders/list');\nconst endpoint = 'admin/orders';\nconst ü = 'é'; http.post<Res>(`admin/orders/${id}/approve`)";
        assert_eq!(
            literal_paths(t, t, &[]),
            vec!["admin/orders/list", "admin/orders", "admin/orders/{p}/approve"]
        );
    }

    #[test]
    fn base_functions_optional_parts_and_called_methods() {
        let m = "const base = (assetId: unknown) => `admin/asset/asset/${assetId}/preventive-maintenance`\nexport const preventiveApi = {\n  fsr(assetId, ticketId, fsrId) {\n    return api.get(`${base(assetId)}/${ticketId}/fsr/${fsrId}`)\n  },\n  saveFsr(assetId, ticketId, fsrId) {\n    const endpoint = `${base(assetId)}/${ticketId}/fsr${editing ? `/${fsrId}` : ''}`\n    return api.post(endpoint, data)\n  },\n  comments(assetId, ticketId) {\n    return api.get(`${base(assetId)}/${ticketId}/comment`)\n  },\n}\n";
        let names: BTreeSet<String> = ["preventiveApi".to_string()].into_iter().collect();
        let own = "await preventiveApi.saveFsr(a, t, id)";
        let paths = literal_paths_in(m, &names, &[], own);
        assert_eq!(
            paths,
            vec![
                "admin/asset/asset/{p}/preventive-maintenance/{p}/fsr",
                "admin/asset/asset/{p}/preventive-maintenance/{p}/fsr/{p}"
            ],
            "only saveFsr, both with and without the id"
        );
    }

    #[test]
    fn kind_paths_and_imported_functions() {
        let m = "export async function list(kind) {\n  return api.get(`admin/system/data-setup/${props.kind}`)\n}\nexport async function other() {\n  return api.get('admin/elsewhere/x')\n}\nexport const load = async () => {\n  return api.get('admin/system/system/selfcare-menu-configuration/')\n}\n";
        let kinds = vec!["department".to_string()];
        assert_eq!(literal_paths(m, m, &kinds)[0], "admin/system/data-setup/department");
        let names: BTreeSet<String> = ["load".to_string()].into_iter().collect();
        assert_eq!(
            literal_paths_in(m, &names, &kinds, ""),
            vec!["admin/system/system/selfcare-menu-configuration"]
        );
    }

    #[test]
    fn flat_kind_maps_and_lookups() {
        let m = "export const organizationEndpoints = {\n  department: 'admin/general/parameter/department',\n  'business-activity': 'admin/general/parameter/business-activity',\n}\nconst base = organizationEndpoints[props.kind]\napi.delete(`${base}/${id}`); api.get(`${organizationEndpoints[kind]}/export`)";
        let bases = keyed_values(m, "department");
        assert_eq!(bases, vec!["admin/general/parameter/department"]);
        assert_eq!(
            indexed_paths(m, &bases),
            vec![
                "admin/general/parameter/department/{p}",
                "admin/general/parameter/department/export"
            ]
        );
    }

    #[test]
    fn kind_props_pick_the_part_of_a_shared_template() {
        let page = "<template><MiscArListModern v-bind=\"$props\" /></template>";
        let route: BTreeMap<String, String> = [("kind".to_string(), "einvoice-request".to_string())].into();
        let child = static_strings(page, "MiscArListModern", &route);
        assert_eq!(child["kind"], "einvoice-request");
        let list = "<template><div><template v-if=\"kind === 'einvoice-request'\"><EInvoiceRows/></template><template v-else-if=\"kind === 'debtor'\"><DebtorRows/></template><Footer v-else /></div></template>";
        let pruned = prune_template(list, &BTreeMap::new(), &child);
        assert!(
            renders_tag(&pruned, "EInvoiceRows")
                && !renders_tag(&pruned, "DebtorRows")
                && !renders_tag(&pruned, "Footer"),
            "{pruned}"
        );
        let other: BTreeMap<String, String> = [("kind".to_string(), "debtor".to_string())].into();
        let pruned = prune_template(list, &BTreeMap::new(), &other);
        assert!(
            !renders_tag(&pruned, "EInvoiceRows")
                && renders_tag(&pruned, "DebtorRows")
                && !renders_tag(&pruned, "Footer"),
            "{pruned}"
        );
        assert_eq!(
            static_strings("<X :kind=\"'a'\" mode=\"view\" :k=\"kind\" />", "X", &other),
            [
                ("kind".to_string(), "a".to_string()),
                ("mode".to_string(), "view".to_string()),
                ("k".to_string(), "debtor".to_string())
            ]
            .into()
        );
    }

    #[test]
    fn script_branches_for_other_kinds_are_left_out() {
        let code = "const r = props.kind === 'customer'\n  ? await customerApi.list(scope(), query)\n  : props.kind === 'portfolio'\n    ? await portfolioApi.list(scope(), query)\n    : await organizationApi.list(props.kind, scope(), query)\nif (props.kind === 'customer') { customerApi.remove(id) }";
        let props: BTreeMap<String, String> = [("kind".to_string(), "department".to_string())].into();
        let pruned = prune_script(code, &props);
        assert!(!mentions_call(&pruned, "remove"), "{pruned}");
        assert!(
            !pruned.contains("customerApi.list")
                && !pruned.contains("portfolioApi.list")
                && pruned.contains("organizationApi.list"),
            "{pruned}"
        );
        let props: BTreeMap<String, String> = [("kind".to_string(), "customer".to_string())].into();
        assert!(prune_script(code, &props).contains("customerApi.list"));
    }

    #[test]
    fn composable_functions_the_page_does_not_use() {
        let code = "export function useForm(props) {\n  async function load() {\n    rows.value = await api.list()\n  }\n  async function runApprove(id) {\n    await api.approve(id)\n  }\n  function confirmAndRun(row) {\n    runApprove(row.id)\n  }\n  async function signOff(sig) {\n    await api.signOff(sig)\n  }\n  onMounted(() => load())\n  return { confirmAndRun, signOff }\n}\n";
        let templates = "<button @click=\"confirmAndRun(row)\">Approve</button>";
        let (defined, wired) = wired_functions(code, templates, "const f = useForm(props)");
        assert!(defined.contains("signOff") && defined.contains("useForm"));
        assert!(
            wired.contains("confirmAndRun") && wired.contains("runApprove"),
            "{wired:?}"
        );
        assert!(!wired.contains("signOff"), "{wired:?}");
    }

    #[test]
    fn only_methods_the_page_calls_count() {
        let m = "export const miscArApi = {\n  async list(kind) {\n    return api.get('a/list')\n  },\n  generate: async (id) => {\n    return api.post(`a/${id}/generate`)\n  },\n}\nexport function other() {\n  return api.get('a/other')\n}\n";
        assert_eq!(enclosing_member(m, 3).as_deref(), Some("list"));
        assert_eq!(enclosing_member(m, 6).as_deref(), Some("generate"));
        assert_eq!(enclosing_member(m, 10).as_deref(), Some("other"));
        let page = "const rows = await miscArApi.list(kind); onGenerate(generate)";
        assert!(mentions_call(page, "list") && mentions_call(page, "generate") && !mentions_call(page, "other"));
    }

    #[test]
    fn boolean_props_hide_parts_of_a_shared_component() {
        let parent = "<template><AnpBookingListModern approval /><Other :compact=\"false\" title=\"x y\" /></template>";
        let f = static_flags(parent, "AnpBookingListModern", &BTreeMap::new());
        assert_eq!(f.get("approval"), Some(&true));
        assert_eq!(
            static_flags(parent, "Other", &BTreeMap::new()).get("compact"),
            Some(&false)
        );
        assert!(
            !static_flags(parent, "Other", &BTreeMap::new()).contains_key("y"),
            "words inside a value are not props"
        );
        // a prop worked out from a flag the page passed down
        let list = "<UiListPage :new-to=\"approval ? null : '/booking/new'\" :title=\"approval ? t('a') : t('b')\" />";
        let g = static_flags(list, "UiListPage", &f);
        assert_eq!(g.get("newTo"), Some(&false));
        assert_eq!(g.get("title"), Some(&true));
        let child = "<template><div><button v-if=\"!approval\">New booking</button><template v-if=\"approval\"><b>Approve</b></template><span v-else>Delete</span></div></template>";
        let pruned = prune_template(child, &f, &BTreeMap::new());
        assert!(
            !pruned.contains("New booking") && !pruned.contains("Delete") && pruned.contains("Approve"),
            "{pruned}"
        );
        let mut off = BTreeMap::new();
        off.insert("approval".to_string(), false);
        let pruned = prune_template(child, &off, &BTreeMap::new());
        assert!(
            pruned.contains("New booking") && pruned.contains("Delete") && !pruned.contains("Approve"),
            "{pruned}"
        );
    }

    #[test]
    fn contracts_that_name_the_page_give_its_paths() {
        let contracts = "export const a = {\n  kind: 'delivery',\n  endpoint: 'admin/eform/delivery',\n  detailRoute: 'admin.e-form.delivery.view',\n  statuses: { x: 1 },\n}\nexport const b = { endpoint: 'admin/eform/renovation', detailRoute: 'admin.e-form.renovation.view' }\n";
        let block = block_naming(contracts, "admin.e-form.delivery.view").unwrap();
        assert!(
            block.contains("admin/eform/delivery") && !block.contains("renovation"),
            "{block}"
        );
        let fields = path_fields(block);
        assert_eq!(fields["endpoint"], "admin/eform/delivery");
        let logic = "api.put(`${contract.endpoint}/${id}/approve`); api.get(`${props.contract.endpoint}/${id}`); api.get(`${other}/x`)";
        assert_eq!(
            joined_paths(logic, &fields),
            vec!["admin/eform/delivery/{p}/approve", "admin/eform/delivery/{p}"]
        );
    }

    #[test]
    fn api_paths_built_at_run_time_are_read() {
        let file = "export const contracts = {\n  'department': { base: 'admin/asset/department', label: 'x' },\n  'customer': { base: 'admin/asset/customer' },\n}\nexport function save(c, id) { return api.post(`${c.base}/${id}`) }\nconst root = 'admin/eform/delivery';\nconst approve = (id) => api.put(`${root}/${id}/approve`);\n";
        let block = keyed_block(file, "department").unwrap();
        assert!(
            block.contains("admin/asset/department") && !block.contains("customer"),
            "{block}"
        );
        assert_eq!(literal_paths(block, file, &[]), vec!["admin/asset/department"]);
        let all = literal_paths(file, file, &[]);
        assert!(all.contains(&"admin/eform/delivery/{p}/approve".to_string()), "{all:?}");
        assert!(!all.iter().any(|p| p.starts_with('{')), "{all:?}");
        assert!(path_matches(
            "admin/eform/delivery/{p}/approve",
            "api/admin/eform/delivery/{delivery}/approve"
        ));
        assert!(path_matches("admin/asset/department", "api/admin/asset/department"));
        assert!(!path_matches("admin/asset/department", "api/admin/asset/customer"));
        assert!(!path_matches("{p}/x", "api/a/x"), "too little in common");
    }

    #[test]
    fn quoted_i18n_keys_are_found() {
        let k = quoted_keys(
            "t(isFooter ? 'message.footer' : 'message.header'); const m = {a: \"labels.serviceType\", b: 'deviceType'}; x('plain words', 'id')",
        );
        assert_eq!(
            k,
            vec!["deviceType", "labels.serviceType", "message.footer", "message.header"]
        );
    }

    #[test]
    fn route_containers_are_not_pages() {
        assert!(is_container("<template><router-view /></template>"));
        assert!(is_container(
            "<template>\n  <div class=\"wrap\">\n    <transition name=\"fade\"><RouterView/></transition>\n  </div>\n</template>\n<script setup></script>"
        ));
        assert!(
            !is_container("<template><div><h1>Assets</h1><router-view/></div></template>"),
            "has text"
        );
        assert!(
            !is_container("<template><AdminTopBar/><router-view/></template>"),
            "has a component"
        );
        assert!(!is_container("<template><div>List</div></template>"), "no child pages");
    }

    #[test]
    fn another_portals_api_is_not_the_pages() {
        let others = vec!["selfcare".to_string(), "contractor".to_string()];
        assert!(of_other_portal("api/selfcare/profile/{projectId}", &others));
        assert!(
            !of_other_portal("api/admin/system/selfcare-menu-configuration", &others),
            "a word inside a part"
        );
        assert!(!of_other_portal("api/admin/orders", &others));
        assert!(!of_other_portal("api/selfcareless", &others), "whole segments only");
    }

    #[test]
    fn possible_actions_need_a_button_on_the_page() {
        let mut f = sample();
        let mut del = ep(
            "DELETE",
            "api/assets/depreciation/{id}",
            &["Record successfully deleted."],
            &[],
        );
        del.possible = true;
        let mut approve = ep("POST", "api/bookings/{id}/approve", &[], &[]);
        approve.possible = true;
        f.endpoints = vec![ep("POST", "api/categories", &[], &[]), del, approve];
        f.literals = vec!["New".into(), "Approve".into()];
        let names: Vec<String> = actions(&f).into_iter().map(|a| a.name).collect();
        assert_eq!(
            names,
            vec!["Add", "Approve"],
            "a shared module's Delete without a Delete button is not this page's action"
        );
        let maybe: Vec<String> = possible_actions(&f).into_iter().map(|a| a.name).collect();
        assert_eq!(maybe, vec!["Delete"]);
    }

    #[test]
    fn layouts_and_shared_screens() {
        assert!(is_layout(
            "<template><v-app><Header/><router-view /></v-app></template>"
        ));
        assert!(is_layout("<template><RouterView></RouterView></template>"));
        assert!(!is_layout("<template><div>Orders</div></template>"));
        assert!(
            is_shared(2, 0),
            "a component behind two routes (route props such as {{kind}}) is a shared screen"
        );
        assert!(is_shared(1, 3));
        assert!(!is_shared(1, 1));
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
                stack: String::new(),
                props: Default::default(),
            },
            labels: BTreeMap::from([("en".to_string(), vec!["B".to_string(), "A".to_string()])]),
            literals: vec!["Save".into()],
            screen_texts: Vec::new(),
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
