//! Screenshots of the running app through headless Chrome (the installed one; nothing is downloaded). Each image
//! has a stored spec (page, params, steps, crop, masks, language, user) so it can be retaken when the page changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
use headless_chrome::{Browser, LaunchOptions, Tab};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{Config, Mask, Step};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShotSpec {
    /// `<module>/<page-slug>-<n>`; the file is guide/img/<lang>/<id>.png.
    pub id: String,
    /// Page key the image belongs to.
    pub page: String,
    /// App path, may hold `:param` / `{param}`.
    pub path: String,
    pub params: BTreeMap<String, String>,
    /// Name from [app.users]; default the first.
    pub user: Option<String>,
    pub steps: Vec<Step>,
    /// CSS selector of the element to capture; the viewport when absent.
    pub crop: Option<String>,
    /// Extra masks on top of [screenshots] mask.
    pub mask: Vec<Mask>,
    pub lang: String,
}

static PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\w+)|\{(\w+)\??\}").unwrap());
static SAFE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]*(/[a-z0-9][a-z0-9_-]*)*$").unwrap());

pub fn validate_spec(s: &ShotSpec) -> Result<(), String> {
    if !SAFE_ID.is_match(&s.id) {
        return Err(format!(
            "shot id {:?} must look like <module>/<page-slug>-<n> (lowercase letters, digits, - and _)",
            s.id
        ));
    }
    if s.lang.is_empty() {
        return Err("shot spec needs lang".into());
    }
    if !s.path.starts_with('/') {
        return Err(format!("shot path {:?} must start with /", s.path));
    }
    Ok(())
}

pub fn resolve_path(s: &ShotSpec) -> Result<String, String> {
    let mut missing = Vec::new();
    let out = PARAM
        .replace_all(&s.path, |c: &regex::Captures| {
            let name = c.get(1).or(c.get(2)).map(|m| m.as_str()).unwrap_or_default();
            match s.params.get(name) {
                Some(v) => v.clone(),
                None => {
                    missing.push(name.to_string());
                    c[0].to_string()
                }
            }
        })
        .into_owned();
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!("page path {} needs params: {}", s.path, missing.join(", ")))
    }
}

/// Document-relative path used in guide sources.
pub fn image_ref(s: &ShotSpec) -> String {
    format!("guide/img/{}/{}.png", s.lang, s.id)
}

pub fn image_path(cfg: &Config, s: &ShotSpec) -> PathBuf {
    cfg.out.join(image_ref(s))
}

fn chrome_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        return Some(PathBuf::from(p));
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

struct Session {
    _browser: Browser,
    tab: Arc<Tab>,
    lang: Option<String>,
}

pub struct Shooter {
    cfg: Config,
    chrome: PathBuf,
    sessions: BTreeMap<String, Session>,
    current: Option<String>,
}

fn mask_js(masks: &[Mask]) -> String {
    let list: Vec<serde_json::Value> = masks
        .iter()
        .map(|m| match m {
            Mask::Selector { selector, text } => serde_json::json!({"selector": selector, "text": text}),
            Mask::Regex { regex, text } => serde_json::json!({"regex": regex, "text": text}),
        })
        .collect();
    format!(
        r#"(function(masks){{
  masks.forEach(function(m){{
    if (m.selector) {{
      document.querySelectorAll(m.selector).forEach(function(e){{
        if ('value' in e && (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA')) {{ e.value = m.text; e.setAttribute('value', m.text); }}
        else {{ e.textContent = m.text; }}
      }});
    }} else if (m.regex) {{
      var re = new RegExp(m.regex, 'g');
      var w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      var n; while ((n = w.nextNode())) {{ if (re.test(n.nodeValue)) {{ re.lastIndex = 0; n.nodeValue = n.nodeValue.replace(re, m.text); }} }}
      document.querySelectorAll('input,textarea').forEach(function(e){{ if (re.test(e.value)) {{ re.lastIndex = 0; e.value = e.value.replace(re, m.text); }} }});
    }}
  }});
  return true;
}})({})"#,
        serde_json::Value::Array(list)
    )
}

impl Shooter {
    pub fn start(cfg: &Config) -> Result<Shooter, String> {
        let chrome = chrome_path().ok_or("Chrome not found; install Google Chrome or set CHROME_PATH")?;
        if cfg.app.base_url.is_empty() {
            return Err("[app] base_url is not set in .doctest.toml".into());
        }
        Ok(Shooter {
            cfg: cfg.clone(),
            chrome,
            sessions: BTreeMap::new(),
            current: None,
        })
    }

    fn creds(&self, user: &str) -> Result<(String, String), String> {
        let Some((_, c)) = self.cfg.app.users.iter().find(|(n, _)| n == user) else {
            return Err(format!("user {user} is not in [app.users]"));
        };
        let get = |v: &str| {
            std::env::var(v).map_err(|_| format!("env var {v} (password or user name for {user}) is not set"))
        };
        Ok((get(&c.user_env)?, get(&c.password_env)?))
    }

    fn run_step(
        tab: &Tab,
        i: usize,
        step: &Step,
        path: &str,
        subst: &dyn Fn(&str) -> String,
        base: &str,
    ) -> Result<(), String> {
        let find = |sel: &str| {
            tab.wait_for_element_with_custom_timeout(sel, Duration::from_secs(10))
                .map_err(|_| format!("step {i}: selector {sel:?} not found on {path}"))
        };
        match step {
            Step::Fill { fill, value } => {
                let el = find(fill)?;
                el.click().map_err(|e| format!("step {i}: {e}"))?;
                // clear, then type, so frameworks see input events
                tab.evaluate(
                    &format!(
                        "(function(){{var e=document.querySelector({});if(e){{e.value='';}}}})()",
                        serde_json::to_string(fill).unwrap_or_default()
                    ),
                    false,
                )
                .map_err(|e| format!("step {i}: {e}"))?;
                tab.type_str(&subst(value)).map_err(|e| format!("step {i}: {e}"))?;
            }
            Step::Select { select, value } => {
                find(select)?;
                let js = format!(
                    "(function(){{var e=document.querySelector({});e.value={};e.dispatchEvent(new Event('change',{{bubbles:true}}));}})()",
                    serde_json::to_string(select).unwrap_or_default(),
                    serde_json::to_string(&subst(value)).unwrap_or_default()
                );
                tab.evaluate(&js, false).map_err(|e| format!("step {i}: {e}"))?;
            }
            Step::Click { click } => {
                find(click)?.click().map_err(|e| format!("step {i}: {e}"))?;
            }
            Step::Wait { wait } => {
                find(wait)?;
            }
            Step::Goto { goto } => {
                tab.navigate_to(&format!("{base}{}", subst(goto)))
                    .and_then(|t| t.wait_until_navigated())
                    .map_err(|e| format!("step {i}: cannot open {goto}: {e}"))?;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
        Ok(())
    }

    fn session(&mut self, user: Option<&str>, lang: &str) -> Result<Arc<Tab>, String> {
        let user = user
            .map(String::from)
            .or_else(|| self.cfg.app.users.first().map(|(n, _)| n.clone()))
            .unwrap_or_default();
        let base = self.cfg.app.base_url.clone();
        if !self.sessions.contains_key(&user) {
            let opts = LaunchOptions::default_builder()
                .path(Some(self.chrome.clone()))
                .headless(true)
                .window_size(Some((1440, 900)))
                .build()
                .map_err(|e| format!("cannot start Chrome: {e}"))?;
            let browser = Browser::new(opts).map_err(|e| format!("cannot start Chrome: {e}"))?;
            let tab = browser.new_tab().map_err(|e| format!("cannot open a tab: {e}"))?;
            tab.set_default_timeout(Duration::from_secs(20));
            let (login, login_steps) = self.cfg.login_for(&user);
            if !user.is_empty() && !login.is_empty() {
                let (u, p) = self.creds(&user)?;
                tab.navigate_to(&format!("{base}{login}"))
                    .and_then(|t| t.wait_until_navigated())
                    .map_err(|e| format!("cannot open the sign-in page {base}{login}: {e}"))?;
                let subst = |v: &str| v.replace("$user", &u).replace("$password", &p);
                for (i, step) in login_steps.iter().enumerate() {
                    Self::run_step(&tab, i + 1, step, &login, &subst, &base)
                        .map_err(|e| format!("signing in as {user}: {e}"))?;
                }
            }
            self.sessions.insert(
                user.clone(),
                Session {
                    _browser: browser,
                    tab,
                    lang: None,
                },
            );
        }
        let locale_steps = self.cfg.app.locale_steps.clone();
        let s = self.sessions.get_mut(&user).expect("inserted");
        if s.lang.as_deref() != Some(lang) && !locale_steps.is_empty() {
            let subst = |v: &str| v.replace("$lang", lang);
            for (i, step) in locale_steps.iter().enumerate() {
                Self::run_step(&s.tab, i + 1, step, "(language switch)", &subst, &base)?;
            }
        }
        s.lang = Some(lang.to_string());
        self.current = Some(user);
        Ok(s.tab.clone())
    }

    /// Takes the screenshot of a spec and writes it (PNG, mode 644, at most max_width wide).
    pub fn capture(&mut self, spec: &ShotSpec) -> Result<PathBuf, String> {
        validate_spec(spec)?;
        let path = resolve_path(spec)?;
        let tab = self.session(spec.user.as_deref(), &spec.lang)?;
        let base = self.cfg.app.base_url.clone();
        tab.navigate_to(&format!("{base}{path}"))
            .and_then(|t| t.wait_until_navigated())
            .map_err(|e| format!("cannot open {base}{path}: {e}"))?;
        std::thread::sleep(Duration::from_millis(400));
        let lang = spec.lang.clone();
        let subst = move |v: &str| v.replace("$lang", &lang);
        for (i, step) in spec.steps.iter().enumerate() {
            Self::run_step(&tab, i + 1, step, &path, &subst, &base)?;
        }
        let masks: Vec<Mask> = self.cfg.screenshots.mask.iter().chain(&spec.mask).cloned().collect();
        tab.evaluate(&mask_js(&masks), false)
            .map_err(|e| format!("masking failed: {e}"))?;
        std::thread::sleep(Duration::from_millis(150));
        let png = match &spec.crop {
            Some(sel) => tab
                .wait_for_element_with_custom_timeout(sel, Duration::from_secs(10))
                .map_err(|_| format!("crop selector {sel:?} not found on {path}"))?
                .capture_screenshot(CaptureScreenshotFormatOption::Png)
                .map_err(|e| format!("screenshot failed: {e}"))?,
            None => tab
                .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
                .map_err(|e| format!("screenshot failed: {e}"))?,
        };
        let mut img = image::load_from_memory(&png).map_err(|e| format!("bad screenshot: {e}"))?;
        let max = self.cfg.screenshots.max_width;
        if img.width() > max {
            let h = (img.height() as u64 * max as u64 / img.width() as u64) as u32;
            img = img.resize(max, h.max(1), image::imageops::FilterType::Lanczos3);
        }
        let out = image_path(&self.cfg, spec);
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        img.save_with_format(&out, image::ImageFormat::Png)
            .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    #[cfg(test)]
    fn current_tab(&self) -> Result<Arc<Tab>, String> {
        let u = self.current.as_ref().ok_or("no page open")?;
        Ok(self.sessions[u].tab.clone())
    }

    #[cfg(test)]
    /// Text of the page as last captured (after masks).
    pub fn page_text(&self) -> Result<String, String> {
        let v = self
            .current_tab()?
            .evaluate("document.body.innerText", false)
            .map_err(|e| e.to_string())?;
        Ok(v.value.and_then(|v| v.as_str().map(String::from)).unwrap_or_default())
    }

    #[cfg(test)]
    pub fn input_value(&self, selector: &str) -> Result<String, String> {
        let js = format!(
            "(document.querySelector({})||{{}}).value",
            serde_json::to_string(selector).unwrap_or_default()
        );
        let v = self.current_tab()?.evaluate(&js, false).map_err(|e| e.to_string())?;
        Ok(v.value.and_then(|v| v.as_str().map(String::from)).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn spec(path: &str) -> ShotSpec {
        ShotSpec {
            id: "orders/shop-orders-create-1".into(),
            page: "shop.orders.create".into(),
            path: path.into(),
            lang: "en".into(),
            ..ShotSpec::default()
        }
    }

    fn cfg_in(dir: &std::path::Path, extra: &str) -> Config {
        std::fs::write(
            dir.join(".doctest.toml"),
            format!("[project]\nname=\"x\"\nlanguages=[\"en\",\"id\"]\n{extra}"),
        )
        .unwrap();
        Config::find(dir).unwrap()
    }

    #[test]
    fn resolve_path_substitutes() {
        let mut s = spec("/lease-out/:id/units/{unit}");
        s.params.insert("id".into(), "7".into());
        s.params.insert("unit".into(), "3".into());
        assert_eq!(resolve_path(&s).unwrap(), "/lease-out/7/units/3");
    }

    #[test]
    fn resolve_path_refuses_missing_param() {
        let err = resolve_path(&spec("/lease-out/:id")).unwrap_err();
        assert_eq!(err, "page path /lease-out/:id needs params: id");
    }

    #[test]
    fn image_path_per_language() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_in(d.path(), "");
        let mut s = spec("/x");
        s.lang = "id".into();
        assert_eq!(
            image_path(&cfg, &s),
            cfg.out.join("guide/img/id/orders/shop-orders-create-1.png")
        );
        assert_eq!(image_ref(&s), "guide/img/id/orders/shop-orders-create-1.png");
    }

    #[test]
    fn spec_id_must_be_safe() {
        let mut s = spec("/x");
        s.id = "../etc/passwd".into();
        assert!(validate_spec(&s).unwrap_err().contains("id"));
    }

    /// Serves tests/fixtures/static-app on 127.0.0.1:<port>.
    fn serve() -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/static-app");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .trim_start_matches('/')
                        .to_string();
                    let body = std::fs::read(root.join(if path.is_empty() { "login.html" } else { &path }))
                        .unwrap_or_default();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&body);
                });
            }
        });
        port
    }

    fn live_cfg(dir: &std::path::Path, port: u16) -> Config {
        // SAFETY: only these ignored tests read these variables; they set the same values.
        unsafe {
            std::env::set_var("DOCTEST_TEST_USER", "demo");
            std::env::set_var("DOCTEST_TEST_PASSWORD", "x");
        }
        cfg_in(
            dir,
            &format!(
                "[app]\nbase_url=\"http://127.0.0.1:{port}\"\nlogin_url=\"/login.html\"\nlogin_steps=[{{fill=\"[name=username]\",value=\"$user\"}},{{fill=\"[name=password]\",value=\"$password\"}},{{click=\"button[type=submit]\"}},{{wait=\".admin-layout\"}}]\n[app.users.admin]\nuser_env=\"DOCTEST_TEST_USER\"\npassword_env=\"DOCTEST_TEST_PASSWORD\"\n[screenshots]\nmax_width=800\nmask=[{{selector=\".customer-name\",text=\"Demo Tenant\"}},{{regex=\"[A-Z]{{2}}-\\\\d{{2}}-\\\\d{{2}}\",text=\"D-01-01\"}}]\n"
            ),
        )
    }

    #[test]
    #[ignore = "needs Chrome"]
    fn captures_masked_png() {
        use std::os::unix::fs::PermissionsExt;
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        let cfg = live_cfg(d.path(), port);
        let mut sh = Shooter::start(&cfg).unwrap();
        let mut s = spec("/orders.html");
        s.steps = vec![crate::config::Step::Click { click: "#more".into() }];
        s.mask = vec![crate::config::Mask::Selector {
            selector: ".email".into(),
            text: "demo@example.com".into(),
        }];
        let out = sh.capture(&s).unwrap();
        assert!(out.is_file());
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o644);
        let img = image::open(&out).unwrap();
        assert!(img.width() <= 800, "{}", img.width());
        let text = sh.page_text().unwrap();
        assert!(
            !text.contains("Real Customer") && text.contains("Demo Tenant"),
            "{text}"
        );
        assert!(!text.contains("AB-12-34") && text.contains("D-01-01"), "{text}");
        assert!(text.contains("demo") && text.contains("Extra panel"));
        assert_eq!(sh.input_value(".email").unwrap(), "demo@example.com");
    }

    #[test]
    #[ignore = "needs Chrome"]
    fn missing_selector_reports_step() {
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        let cfg = live_cfg(d.path(), port);
        let mut sh = Shooter::start(&cfg).unwrap();
        let mut s = spec("/orders.html");
        s.steps = vec![crate::config::Step::Click { click: "#nope".into() }];
        let err = sh.capture(&s).unwrap_err();
        assert!(
            err.contains("step 1: selector \"#nope\" not found on /orders.html"),
            "{err}"
        );
    }

    #[test]
    #[ignore = "needs Chrome"]
    fn crop_to_element() {
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        let cfg = live_cfg(d.path(), port);
        let mut sh = Shooter::start(&cfg).unwrap();
        let mut s = spec("/orders.html");
        s.crop = Some(".card".into());
        let img = image::open(sh.capture(&s).unwrap()).unwrap();
        assert!(img.height() < 200, "{}", img.height());
    }
}
