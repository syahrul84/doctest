//! `.doctest.toml`: where the apps are, the running app used for screenshots, how tests run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const FILE: &str = ".doctest.toml";

#[derive(Debug, Clone)]
pub struct Config {
    /// Folder holding `.doctest.toml` (the backend repo).
    pub root: PathBuf,
    pub name: String,
    pub backend: PathBuf,
    pub frontends: Vec<PathBuf>,
    pub out: PathBuf,
    pub languages: Vec<String>,
    pub app: AppConfig,
    pub screenshots: ShotConfig,
    pub tests: TestConfig,
    pub guide: GuideConfig,
    /// Parallel workers: AI workers claiming items, and tool calls at the same time.
    pub workers: usize,
    /// Also read pages with `workers` codemap clients at once. Off by default: on a large app four clients were 2.5×
    /// slower than one (codemap is CPU-bound per call).
    pub parallel_reads: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub base_url: String,
    pub login_url: String,
    pub users: Vec<(String, UserCreds)>,
    pub login_steps: Vec<Step>,
    pub locale_steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct UserCreds {
    pub user_env: String,
    pub password_env: String,
    /// This user's sign-in page and steps when they differ from [app] (another portal).
    #[serde(default)]
    pub login_url: Option<String>,
    #[serde(default)]
    pub login_steps: Option<Vec<Step>>,
}

/// One audience's guide (admin, tenant, vendor…): the pages it holds and the user its screenshots sign in as.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Portal {
    pub name: String,
    pub title: String,
    pub pages: Vec<String>,
    /// Only for unattended runs: the [app.users] entry whose env vars sign in.
    pub user: Option<String>,
    /// The portal's sign-in page; none for a public area.
    pub login_url: Option<String>,
}

/// One browser action. Written in TOML as `{ fill = "sel", value = "x" }`, `{ click = "sel" }` …
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Step {
    /// Clears the field, then types the value.
    Fill {
        fill: String,
        value: String,
    },
    Select {
        select: String,
        value: String,
    },
    Click {
        click: String,
    },
    /// Waits (up to 10 s) until this text is on the page.
    WaitText {
        wait_text: String,
    },
    /// Waits (up to 10 s) for an element.
    Wait {
        wait: String,
    },
    /// Scrolls an element into view (a CSS selector), or `top` / `bottom` of the page.
    Scroll {
        scroll: String,
    },
    Goto {
        goto: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Mask {
    /// Replaces the text (or input value) of every element matching the selector, inside shadow roots too.
    Selector { selector: String, text: String },
    /// Replaces text matching a JavaScript regular expression (`(?i)` at the start: ignore case).
    Regex { regex: String, text: String },
    /// Hides every element matching the selector (display:none), inside shadow roots too.
    Hide { hide: String },
}

#[derive(Debug, Clone)]
pub struct ShotConfig {
    pub max_width: u32,
    pub mask: Vec<Mask>,
    /// Safe mode (default on): while a screenshot is taken the browser refuses POST/PUT/PATCH/DELETE requests, so
    /// a step cannot save, delete or send anything. A spec opts out with `allow_saves`.
    pub safe: bool,
    /// Requests allowed in safe mode, e.g. `POST /api/*/search` (method optional; `*` matches any part).
    pub allow: Vec<String>,
    /// Retake screenshots in every language (default off: translations reuse the primary language's images).
    pub per_language: bool,
}

#[derive(Debug, Clone)]
pub struct TestConfig {
    /// Command with `{filter}` where doctest puts `--filter '<regex>'`.
    pub run: String,
    pub unit_dir: PathBuf,
    pub feature_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GuideConfig {
    pub modules_from: Option<PathBuf>,
    pub allow_bold: Vec<String>,
    /// A page guide needs a section per action (Add, Edit, Delete, Approve…).
    pub require_actions: bool,
    /// Images a page guide needs at least; default 1 + one per action.
    pub min_screenshots: Option<usize>,
    /// Only pages whose route name or path matches one of these (`admin.*`, `/admin/*`); empty = all.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Separate guides per portal, in order; empty = one guide.
    pub portals: Vec<Portal>,
    /// Where the server publishes document/guide (for the start page).
    pub publish_path: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    project: RawProject,
    #[serde(default)]
    app: RawApp,
    #[serde(default)]
    screenshots: RawShots,
    #[serde(default)]
    tests: RawTests,
    #[serde(default)]
    guide: RawGuide,
    #[serde(default)]
    work: RawWork,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawWork {
    workers: Option<usize>,
    parallel_reads: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProject {
    #[serde(default)]
    name: String,
    backend: Option<String>,
    #[serde(default)]
    frontends: Vec<String>,
    out: Option<String>,
    languages: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawApp {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    login_url: String,
    #[serde(default)]
    users: toml::map::Map<String, toml::Value>,
    #[serde(default)]
    login_steps: Vec<Step>,
    #[serde(default)]
    locale_steps: Vec<Step>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawShots {
    max_width: Option<u32>,
    #[serde(default)]
    mask: Vec<Mask>,
    safe: Option<bool>,
    #[serde(default)]
    allow: Vec<String>,
    per_language: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTests {
    run: Option<String>,
    unit_dir: Option<String>,
    feature_dir: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGuide {
    modules_from: Option<String>,
    #[serde(default)]
    allow_bold: Vec<String>,
    require_actions: Option<bool>,
    min_screenshots: Option<usize>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    portals: toml::map::Map<String, toml::Value>,
    publish_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPortal {
    title: Option<String>,
    #[serde(default)]
    pages: Vec<String>,
    user: Option<String>,
    login_url: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let raw: RawFile = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let rel = |s: &str| {
            if s.is_empty() || s == "." {
                root.clone()
            } else {
                root.join(s)
            }
        };
        let mut users = Vec::new();
        for (name, value) in raw.app.users {
            let creds: UserCreds = value
                .try_into()
                .map_err(|e| format!("{}: [app.users.{name}]: {e}", path.display()))?;
            users.push((name, creds));
        }
        let mut portals = Vec::new();
        for (name, value) in raw.guide.portals {
            let rp: RawPortal = value
                .try_into()
                .map_err(|e| format!("{}: [guide.portals.{name}]: {e}", path.display()))?;
            if rp.pages.is_empty() {
                return Err(format!(
                    "{}: [guide.portals.{name}] needs pages = [\"admin.*\", …]",
                    path.display()
                ));
            }
            let title = rp.title.unwrap_or_else(|| {
                let mut c = name.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
                    + " guide"
            });
            portals.push(Portal {
                name: name.clone(),
                title,
                pages: rp.pages,
                user: rp.user,
                login_url: rp.login_url,
            });
        }
        let run = raw.tests.run.unwrap_or_else(|| "php artisan test {filter}".into());
        if !run.contains("{filter}") {
            return Err(format!(
                "{}: [tests] run must contain {{filter}} (where doctest puts --filter): {run}",
                path.display()
            ));
        }
        let languages = raw
            .project
            .languages
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| vec!["en".into()]);
        Ok(Config {
            name: if raw.project.name.is_empty() {
                root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                raw.project.name
            },
            backend: rel(raw.project.backend.as_deref().unwrap_or(".")),
            frontends: raw.project.frontends.iter().map(|f| root.join(f)).collect(),
            out: rel(raw.project.out.as_deref().unwrap_or("document")),
            languages,
            app: AppConfig {
                base_url: raw.app.base_url.trim_end_matches('/').to_string(),
                login_url: raw.app.login_url,
                users,
                login_steps: raw.app.login_steps,
                locale_steps: raw.app.locale_steps,
            },
            screenshots: ShotConfig {
                max_width: raw.screenshots.max_width.unwrap_or(1600),
                mask: raw.screenshots.mask,
                safe: raw.screenshots.safe.unwrap_or(true),
                allow: raw.screenshots.allow,
                per_language: raw.screenshots.per_language.unwrap_or(false),
            },
            tests: TestConfig {
                run,
                unit_dir: PathBuf::from(raw.tests.unit_dir.unwrap_or_else(|| "tests/Unit".into())),
                feature_dir: PathBuf::from(raw.tests.feature_dir.unwrap_or_else(|| "tests/Feature".into())),
            },
            guide: GuideConfig {
                modules_from: raw.guide.modules_from.map(|m| root.join(m)),
                allow_bold: raw.guide.allow_bold,
                require_actions: raw.guide.require_actions.unwrap_or(true),
                min_screenshots: raw.guide.min_screenshots,
                include: raw.guide.include,
                exclude: raw.guide.exclude,
                portals,
                publish_path: raw.guide.publish_path.unwrap_or_else(|| "/docs".into()),
            },
            workers: raw.work.workers.unwrap_or(1).clamp(1, 32),
            parallel_reads: raw.work.parallel_reads.unwrap_or(false),
            root,
        })
    }

    /// `.doctest.toml` in `start` or the nearest folder above it.
    pub fn find(start: &Path) -> Result<Config, String> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(FILE);
            if candidate.is_file() {
                return Config::load(&candidate);
            }
            dir = d.parent();
        }
        Err(format!(
            "no {FILE} in {} or above; run `doctest init` in the backend repo",
            start.display()
        ))
    }

    /// The portal a page belongs to: the first whose patterns match; "other" when none does; "" without portals.
    pub fn portal_of(&self, key: &str, path: &str) -> String {
        crate::portals::portal_in(&self.guide.portals, key, path)
    }

    /// Sign-in page and steps for a portal (or an [app.users] name): the portal's login_url, the user's own, else
    /// [app]; "public" needs none.
    pub fn login_for(&self, name: &str) -> (String, Vec<Step>) {
        if name == "public" {
            return (String::new(), Vec::new());
        }
        let portal = self.guide.portals.iter().find(|p| p.name == name);
        let user = self.creds_user(name);
        let u = user
            .as_deref()
            .and_then(|n| self.app.users.iter().find(|(x, _)| x == n))
            .map(|(_, c)| c);
        (
            portal
                .and_then(|p| p.login_url.clone())
                .or_else(|| u.and_then(|c| c.login_url.clone()))
                .unwrap_or_else(|| self.app.login_url.clone()),
            u.and_then(|c| c.login_steps.clone())
                .unwrap_or_else(|| self.app.login_steps.clone()),
        )
    }

    /// The [app.users] entry whose env vars sign in as this portal or user (unattended runs only).
    pub fn creds_user(&self, name: &str) -> Option<String> {
        self.guide
            .portals
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.user.clone())
            .or_else(|| self.app.users.iter().find(|(n, _)| n == name).map(|(n, _)| n.clone()))
    }

    /// This config with these portals (detected ones when .doctest.toml names none).
    pub fn with_portals(&self, portals: Vec<Portal>) -> Config {
        let mut c = self.clone();
        if c.guide.portals.is_empty() {
            c.guide.portals = portals;
        }
        c
    }

    /// Every sign-in `doctest login` can do: (portal, login page) for each portal with one, else ("default",
    /// [app] login_url) when there are no portals.
    pub fn login_targets(&self) -> Vec<(String, String)> {
        if self.guide.portals.is_empty() {
            return if self.app.login_url.is_empty() {
                Vec::new()
            } else {
                vec![("default".into(), self.app.login_url.clone())]
            };
        }
        self.guide
            .portals
            .iter()
            .map(|p| (p.name.clone(), self.login_for(&p.name).0))
            .filter(|(_, url)| !url.is_empty())
            .collect()
    }

    pub fn primary_language(&self) -> &str {
        &self.languages[0]
    }

    /// Where a frontend's code lives: its `src/` folder, else the folder itself (e.g. Laravel's resources/js).
    pub fn src_root(fe: &Path) -> PathBuf {
        let src = fe.join("src");
        if src.is_dir() { src } else { fe.to_path_buf() }
    }

    /// `src_root` relative to the frontend ("src" or "").
    pub fn src_rel(fe: &Path) -> PathBuf {
        Config::src_root(fe)
            .strip_prefix(fe)
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Short name of a repo (its folder name), the key used in state.json.
    pub fn repo_name(path: &Path) -> String {
        path.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                _ => None,
            })
            .next_back()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const FULL: &str = r#"
[project]
name = "shop"
frontends = ["../shop-web"]
languages = ["en", "id"]

[app]
base_url = "http://localhost:8196"
login_url = "/session/admin/login"
login_steps = [
  { fill = "input[name=username]", value = "$user" },
  { fill = "input[name=password]", value = "$password" },
  { click = "button[type=submit]" },
  { wait = ".admin-layout" },
]

[app.users.admin]
user_env = "DOCTEST_USER"
password_env = "DOCTEST_PASSWORD"

[screenshots]
max_width = 1600
mask = [
  { selector = ".tenant-name", text = "Demo Tenant" },
  { regex = "\\b[A-Z]{2}-\\d{2}\\b", text = "D-01" },
]

[tests]
run = "php artisan test {filter}"

[guide]
modules_from = "../shop-web/src/config/adminMenu.ts"
allow_bold = ["Note"]
"#;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join(".doctest.toml");
        fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn loads_full_example() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::load(&write(d.path(), FULL)).unwrap();
        assert_eq!(cfg.backend, d.path());
        assert_eq!(cfg.frontends, vec![d.path().join("../shop-web")]);
        assert_eq!(cfg.languages, vec!["en", "id"]);
        assert_eq!(cfg.primary_language(), "en");
        assert_eq!(cfg.app.users[0].0, "admin");
        assert_eq!(cfg.app.users[0].1.user_env, "DOCTEST_USER");
        assert!(matches!(cfg.app.login_steps[2], Step::Click { .. }));
        assert!(cfg.tests.run.contains("{filter}"));
        assert_eq!(cfg.screenshots.max_width, 1600);
        assert!(matches!(cfg.screenshots.mask[1], Mask::Regex { .. }));
        assert_eq!(cfg.guide.allow_bold, vec!["Note"]);
    }

    #[test]
    fn defaults_when_sections_missing() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::load(&write(d.path(), "[project]\nname = \"x\"\n")).unwrap();
        assert_eq!(cfg.out, d.path().join("document"));
        assert_eq!(cfg.languages, vec!["en"]);
        assert_eq!(cfg.screenshots.max_width, 1600);
        assert_eq!(cfg.tests.unit_dir, PathBuf::from("tests/Unit"));
        assert_eq!(cfg.tests.feature_dir, PathBuf::from("tests/Feature"));
        assert!(cfg.tests.run.contains("{filter}"));
    }

    #[test]
    fn rejects_run_without_filter_placeholder() {
        let d = tempfile::tempdir().unwrap();
        let err = Config::load(&write(
            d.path(),
            "[project]\nname=\"x\"\n[tests]\nrun = \"php artisan test\"\n",
        ))
        .unwrap_err();
        assert!(err.contains("{filter}"), "{err}");
    }

    #[test]
    fn portals_and_per_user_login() {
        let d = tempfile::tempdir().unwrap();
        let toml = "[project]\nname=\"x\"\n[app]\nlogin_url=\"/admin/login\"\n[app.users.admin]\nuser_env=\"A\"\npassword_env=\"B\"\n[app.users.tenant]\nuser_env=\"C\"\npassword_env=\"D\"\nlogin_url=\"/selfcare/login\"\nlogin_steps=[{click=\"#go\"}]\n[guide.portals.admin]\ntitle=\"Admin guide\"\npages=[\"admin.*\"]\nuser=\"admin\"\n[guide.portals.tenant]\npages=[\"selfcare.*\", \"/selfcare/*\"]\nuser=\"tenant\"\n";
        let cfg = Config::load(&write(d.path(), toml)).unwrap();
        let names: Vec<&str> = cfg.guide.portals.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["admin", "tenant"]);
        assert_eq!(cfg.guide.portals[1].title, "Tenant guide");
        assert_eq!(cfg.portal_of("selfcare.home", "/selfcare"), "tenant");
        assert_eq!(cfg.portal_of("x", "/selfcare/bills"), "tenant");
        assert_eq!(cfg.portal_of("contractor.home", "/contractor"), "public");
        assert_eq!(cfg.creds_user("tenant").as_deref(), Some("tenant"));
        let (url, steps) = cfg.login_for("tenant");
        assert_eq!(url, "/selfcare/login");
        assert_eq!(steps.len(), 1);
        assert_eq!(cfg.login_for("admin").0, "/admin/login");
    }

    #[test]
    fn sign_in_per_portal() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::load(&write(
            d.path(),
            "[project]\nname=\"x\"\n[app]\nbase_url=\"http://x\"\nlogin_url=\"/login\"\n",
        ))
        .unwrap();
        let p = |name: &str, login: Option<&str>| Portal {
            name: name.into(),
            title: format!("{name} guide"),
            pages: vec![format!("{name}.*")],
            user: None,
            login_url: login.map(String::from),
        };
        let cfg = cfg.with_portals(vec![
            p("admin", Some("/session/admin/login")),
            p("selfcare", Some("/session/selfcare/login")),
            p("kiosk", None),
        ]);
        assert_eq!(cfg.login_for("selfcare").0, "/session/selfcare/login");
        assert_eq!(
            cfg.login_for("kiosk").0,
            "/login",
            "a portal without its own login page uses [app] login_url"
        );
        assert_eq!(cfg.login_for("public").0, "", "public pages need no sign-in");
        let targets: Vec<(String, String)> = cfg.login_targets();
        assert_eq!(targets[0], ("admin".to_string(), "/session/admin/login".to_string()));
        assert_eq!(targets.len(), 3);
        let flat = Config::load(&write(d.path(), "[project]\nname=\"x\"\n[app]\nlogin_url=\"/login\"\n")).unwrap();
        assert_eq!(
            flat.login_targets(),
            vec![("default".to_string(), "/login".to_string())]
        );
        let none = Config::load(&write(d.path(), "[project]\nname=\"x\"\n")).unwrap();
        assert!(none.login_targets().is_empty());
    }

    #[test]
    fn no_portals_means_no_portal_folder() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::load(&write(d.path(), "[project]\nname=\"x\"\n")).unwrap();
        assert_eq!(cfg.portal_of("admin.x", "/admin/x"), "");
    }

    #[test]
    fn find_walks_up() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "[project]\nname = \"x\"\n");
        let deep = d.path().join("app/Http");
        fs::create_dir_all(&deep).unwrap();
        let cfg = Config::find(&deep).unwrap();
        assert_eq!(cfg.root, d.path());
    }
}
