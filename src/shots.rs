//! Screenshots of the running app through headless Chrome (the installed one; nothing is downloaded). Each image
//! has a stored spec (page, params, steps, crop, masks, language, user) so it can be retaken when the page changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
use headless_chrome::{Browser, LaunchOptions, Tab};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::{Config, Mask, Step};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShotSpec {
    /// `<module>/<page-slug>-<n>`; the file is guide/img/<lang>/<id>.webp.
    pub id: String,
    /// Page key the image belongs to.
    pub page: String,
    /// App path, may hold `:param` / `{param}`.
    pub path: String,
    #[serde(deserialize_with = "params_as_text")]
    pub params: BTreeMap<String, String>,
    /// Name from [app.users]; default the first.
    pub user: Option<String>,
    pub steps: Vec<Step>,
    /// CSS selector of the element to capture; the viewport when absent.
    pub crop: Option<String>,
    /// Extra masks on top of [screenshots] mask.
    pub mask: Vec<Mask>,
    pub lang: String,
    /// Let the steps save data (safe mode blocks POST/PUT/PATCH/DELETE otherwise). Re-shooting repeats the save.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_saves: bool,
}

/// What a capture did besides writing the image.
#[derive(Debug, Default)]
pub struct Shot {
    pub path: PathBuf,
    /// Requests safe mode refused (`POST /api/x`).
    pub blocked: Vec<String>,
    /// Masks that matched nothing on the page.
    pub unmatched_masks: Vec<String>,
    /// Dialogs the page opened and doctest answered (`alert "Saved" (OK)`).
    pub dialogs: Vec<String>,
}

/// Route params as text: `{"id": 8}` is read as `{"id": "8"}`.
fn params_as_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    use serde::Deserialize;
    let raw: BTreeMap<String, serde_json::Value> = BTreeMap::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let text = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            (k, text)
        })
        .collect())
}

/// Is this request allowed in safe mode? Reads (GET, HEAD, OPTIONS) always are; others only when a pattern in
/// [screenshots] allow matches (`POST /api/*/search`, or a path alone for any method).
pub fn allowed(method: &str, url: &str, patterns: &[String]) -> bool {
    let method = method.to_ascii_uppercase();
    if matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        return true;
    }
    let path = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split_once('/'))
        .map(|(_, p)| format!("/{p}"))
        .unwrap_or_else(|| url.to_string());
    let path = path.split(['?', '#']).next().unwrap_or_default().to_string();
    patterns.iter().any(|p| {
        let (m, pat) = match p.split_once(' ') {
            Some((m, rest)) if m.chars().all(|c| c.is_ascii_alphabetic()) => {
                (Some(m.to_ascii_uppercase()), rest.trim())
            }
            _ => (None, p.trim()),
        };
        if m.as_ref().is_some_and(|m| *m != method) {
            return false;
        }
        let pat = if pat.starts_with('/') {
            pat.to_string()
        } else {
            format!("/{pat}")
        };
        let re = format!("^{}$", regex::escape(&pat).replace(r"\*", ".*"));
        Regex::new(&re).is_ok_and(|r| r.is_match(&path))
    })
}

/// Blocks saves while armed and remembers what it blocked.
#[derive(Default)]
struct Guard {
    armed: AtomicBool,
    blocked: Mutex<Vec<String>>,
    allow: Vec<String>,
    /// Requests the app's servers answered 401 or 419 during the current shot: what makes an app drop its sign-in.
    refused: Mutex<Vec<String>>,
    /// Dialogs the page opened during the current shot (alert, confirm, "Leave site?"), answered so they cannot block.
    dialogs: Mutex<Vec<String>>,
}

/// Marks a shot that ended on the sign-in page, so capture can retry it in a fresh browser.
const SIGNED_OUT: &str = "the app sent the screenshot to its sign-in page";

/// Why a shot ended on the sign-in page, from the requests the app's servers refused (401/419) and the ones safe mode
/// refused during it. Each browser starts from a copy of the saved sign-in, so the saved one is never touched.
fn signed_out_error(user: &str, refused: &[String], blocked: &[String]) -> String {
    let login = if user == "default" {
        "doctest login".to_string()
    } else {
        format!("doctest login --user {user}")
    };
    let mut refused: Vec<&String> = refused.iter().collect();
    refused.dedup();
    if !refused.is_empty() {
        let list: Vec<&str> = refused.iter().take(5).map(|s| s.as_str()).collect();
        format!(
            "{SIGNED_OUT} after the server refused the sign-in on {} — the app clears its sign-in on any 401. The saved \
             session for {user} is still there and the other shots keep using it, so do not stop the run: this page calls \
             an endpoint {user} may not use (an app bug, or a page of another portal), so tell the person and skip this \
             shot. Run `{login}` only if every page fails.",
            list.join(", ")
        )
    } else if !blocked.is_empty() {
        format!(
            "{SIGNED_OUT} after safe mode refused {} — if that is a sign-in refresh, heartbeat or search, add it to \
             [screenshots] allow; the saved session for {user} is still there and the other shots keep using it.",
            blocked.join(", ")
        )
    } else {
        format!(
            "{SIGNED_OUT} with no refused request seen: the saved session for {user} may have expired — run `{login}` \
             again"
        )
    }
}

/// Every Chrome doctest starts carries `--doctest-owner=<pid>`, so Chromes left behind by a doctest that died can
/// be found and stopped.
static OWNER_ARG: LazyLock<String> = LazyLock::new(|| format!("--doctest-owner={}", std::process::id()));

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Chromes started by doctest: (chrome pid, owner pid).
fn doctest_chromes() -> Vec<(u32, u32)> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let (pid, cmd) = l.split_once(' ')?;
            let owner = cmd
                .split_whitespace()
                .find_map(|a| a.strip_prefix("--doctest-owner="))?;
            Some((pid.parse().ok()?, owner.parse().ok()?))
        })
        .collect()
}

fn kill(pids: &[u32]) {
    if pids.is_empty() {
        return;
    }
    let mut c = std::process::Command::new("kill");
    c.arg("-9");
    for p in pids {
        c.arg(p.to_string());
    }
    let _ = c.stderr(std::process::Stdio::null()).status();
}

/// Stops Chromes whose doctest is gone (killed, crashed, its client closed). Returns how many.
pub fn sweep_orphans() -> usize {
    let dead: Vec<u32> = doctest_chromes()
        .into_iter()
        .filter(|(_, o)| !alive(*o))
        .map(|(p, _)| p)
        .collect();
    kill(&dead);
    dead.len()
}

/// Stops every Chrome this doctest started (on exit, or when the MCP client closes stdin).
pub fn kill_own() {
    let me = std::process::id();
    let own: Vec<u32> = doctest_chromes()
        .into_iter()
        .filter(|(_, o)| *o == me)
        .map(|(p, _)| p)
        .collect();
    kill(&own);
}

/// Errors that mean Chrome went away (crashed, closed, the DevTools connection dropped): start a new one.
fn dropped(e: &str) -> bool {
    let l = e.to_lowercase();
    [
        "connection is closed",
        "underlying connection",
        "channel closed",
        "target closed",
        "no such target",
        "transport loop",
        "websocket",
    ]
    .iter()
    .any(|w| l.contains(w))
}

/// Waits until the page stops loading resources (data requests of a single-page app included), up to `max`.
fn settle(tab: &Tab, max: Duration) {
    let end = std::time::Instant::now() + max;
    let mut last = -1i64;
    let mut stable = 0;
    while std::time::Instant::now() < end {
        let n = tab
            .evaluate(
                "document.readyState === 'complete' ? performance.getEntriesByType('resource').length : -2",
                false,
            )
            .ok()
            .and_then(|v| v.value)
            .and_then(|v| v.as_i64())
            .unwrap_or(-2);
        if n >= 0 && n == last {
            stable += 1;
            if stable >= 3 {
                return;
            }
        } else {
            stable = 0;
        }
        last = n;
        std::thread::sleep(Duration::from_millis(200));
    }
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
    format!("guide/img/{}/{}.webp", s.lang, s.id)
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
    guard: Arc<Guard>,
    _browser: Browser,
    tab: Arc<Tab>,
    lang: Option<String>,
    /// A private copy of the saved sign-in profile (one Chrome profile cannot be open twice).
    _profile: Option<tempfile::TempDir>,
}

/// The user screenshots sign in as when a spec and its portal name none: the first [app.users], else "default".
pub fn default_user(cfg: &Config) -> String {
    cfg.app
        .users
        .first()
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| "default".into())
}

/// doctest's own Chrome profile for a project and user, holding the session `doctest login` saved.
pub fn profile_dir(cfg: &Config, user: &str) -> PathBuf {
    let project = &crate::hash::sha256(&cfg.root.to_string_lossy())[..12];
    crate::reports::tool_root()
        .join("chrome")
        .join(project)
        .join(crate::source::slug(user))
}

const SIGNED_IN: &str = ".doctest-signed-in";

/// `doctest login` saved a session for this user.
pub fn signed_in(cfg: &Config, user: &str) -> bool {
    profile_dir(cfg, user).join(SIGNED_IN).is_file()
}

/// Copies a Chrome profile without its caches and lock files.
pub fn copy_profile(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.contains("Cache")
            || name.starts_with("Singleton")
            || name == "Crashpad"
            || name == "lockfile"
            || name.ends_with(".lock")
        {
            continue;
        }
        let (src, dst) = (e.path(), to.join(&name));
        if src.is_dir() {
            copy_profile(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// How long one Chrome call may go unanswered before doctest gives up on it (a page stuck on a dialog or a load that
/// never ends). The Chrome library also closes a browser that sends nothing for this long; the next shot starts a new
/// one (see `dropped`).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A shot that takes longer than this stops with the step it was on.
const SHOT_LIMIT: Duration = Duration::from_secs(90);

fn launch(
    chrome: &Path,
    headless: bool,
    profile: Option<&Path>,
    call_timeout: Duration,
) -> Result<(Browser, Arc<Tab>), String> {
    let owner = std::ffi::OsStr::new(OWNER_ARG.as_str());
    // a port the system says is free; the Chrome library's own pick (8000-9000) can be the app's port
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .ok();
    let opts = LaunchOptions::default_builder()
        .path(Some(chrome.to_path_buf()))
        .port(port)
        .args(vec![owner])
        .headless(headless)
        .window_size(Some((1440, 900)))
        .user_data_dir(profile.map(Path::to_path_buf))
        .idle_browser_timeout(call_timeout)
        .build()
        .map_err(|e| format!("cannot start Chrome: {e}"))?;
    let browser = Browser::new(opts).map_err(|e| format!("cannot start Chrome: {e}"))?;
    let tab = browser.new_tab().map_err(|e| format!("cannot open a tab: {e}"))?;
    tab.set_default_timeout(Duration::from_secs(20));
    Ok((browser, tab))
}

/// Opens a URL in the tab. Waiting for the load to finish can time out on pages that keep loading (a request that
/// never ends, a leave-page prompt answered late); then doctest tries once more and, if Chrome took the URL, goes on
/// and lets `settle` wait for the page.
fn open(tab: &Tab, url: &str) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..2 {
        match tab.navigate_to(url) {
            Err(e) => last = e.to_string(),
            Ok(t) => match t.wait_until_navigated() {
                Ok(_) => return Ok(()),
                Err(e) => last = e.to_string(),
            },
        }
        let here = tab.get_url();
        if here.split(['?', '#']).next() == url.split(['?', '#']).next() && !here.starts_with("about:") {
            return Ok(());
        }
    }
    Err(format!("cannot open {url}: {last}"))
}

fn on_login_page(url: &str, login_url: &str) -> bool {
    let path = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split_once('/'))
        .map(|(_, p)| format!("/{p}"))
        .unwrap_or_default();
    let path = path.split(['?', '#']).next().unwrap_or_default();
    !login_url.is_empty() && (path == login_url || path.trim_end_matches('/') == login_url.trim_end_matches('/'))
}

/// `doctest login`: opens Chrome on the app's sign-in page with doctest's own profile for `user`, waits until the
/// person has signed in (the page leaves the sign-in URL; OTP and all), and keeps that session for screenshots.
/// `driver` lets tests sign in through the same window.
pub fn login(cfg: &Config, user: &str, headless: bool, driver: &dyn Fn(&Tab)) -> Result<String, String> {
    let chrome = chrome_path().ok_or("Chrome not found; install Google Chrome or set CHROME_PATH")?;
    if cfg.app.base_url.is_empty() {
        return Err("[app] base_url is not set in .doctest.toml".into());
    }
    let (login_url, _) = cfg.login_for(user);
    let profile = profile_dir(cfg, user);
    std::fs::create_dir_all(&profile).map_err(|e| format!("cannot create {}: {e}", profile.display()))?;
    let _ = std::fs::remove_file(profile.join(SIGNED_IN));
    // a person signs in by hand and doctest sends Chrome nothing meanwhile: keep the connection for the whole wait
    let (browser, tab) = launch(&chrome, headless, Some(&profile), Duration::from_secs(900))?;
    let start = format!("{}{}", cfg.app.base_url, login_url);
    tab.navigate_to(&start)
        .and_then(|t| t.wait_until_navigated())
        .map_err(|e| format!("cannot open {start}: {e}"))?;
    driver(&tab);
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let mut away = 0;
    let mut last = start.clone();
    loop {
        std::thread::sleep(Duration::from_millis(700));
        let url = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tab.get_url())) {
            Ok(u) if !u.is_empty() => u,
            _ => break, // the window was closed
        };
        if !url.starts_with("about:") {
            last = url.clone();
        }
        away = if on_login_page(&url, &login_url) || url.starts_with("about:") {
            0
        } else {
            away + 1
        };
        if away >= 3 || std::time::Instant::now() > deadline {
            break;
        }
    }
    if on_login_page(&last, &login_url) || last.starts_with("about:") {
        return Err(format!(
            "not signed in: the window closed or 10 minutes passed on {start}"
        ));
    }
    // let the app finish storing its token, then close Chrome cleanly so the session is written to disk
    std::thread::sleep(Duration::from_secs(2));
    let _ = tab.call_method(headless_chrome::protocol::cdp::Browser::Close(None));
    std::thread::sleep(Duration::from_secs(1));
    drop(browser);
    std::fs::write(profile.join(SIGNED_IN), chrono::Utc::now().to_rfc3339()).map_err(|e| e.to_string())?;
    Ok(user.to_string())
}

/// Cuts an element's box (viewport CSS pixels and the device pixel ratio: [left, top, width, height, ratio]) out of a
/// viewport screenshot; the part outside the screen is left out.
fn crop_png(png: &[u8], rect: &[f64]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png).map_err(|e| e.to_string())?;
    let [l, t, w, h, ratio] = <[f64; 5]>::try_from(rect).map_err(|_| "bad element box".to_string())?;
    let (iw, ih) = (img.width() as f64, img.height() as f64);
    let x0 = (l * ratio).max(0.0).min(iw);
    let y0 = (t * ratio).max(0.0).min(ih);
    let x1 = ((l + w) * ratio).max(0.0).min(iw);
    let y1 = ((t + h) * ratio).max(0.0).min(ih);
    if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
        return Err("the element has no visible area on the screen".into());
    }
    let part = img.crop_imm(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
    let mut out = std::io::Cursor::new(Vec::new());
    part.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// One colour all over (a page that had not drawn, or a crop of nothing).
fn is_blank(img: &image::DynamicImage) -> bool {
    let rgb = img.to_rgb8();
    let mut px = rgb.pixels();
    let Some(first) = px.next() else { return true };
    let close = |a: u8, b: u8| a.abs_diff(b) <= 3;
    px.step_by(7).all(|p| (0..3).all(|c| close(p[c], first[c])))
}

/// Writes a screenshot as lossless WebP (libwebp): the same pixels as the PNG Chrome returns, at about a third of
/// the size of an optimised PNG. No alpha channel (screens are opaque).
pub fn write_image(img: &image::DynamicImage, out: &Path) -> Result<(), String> {
    let rgb = img.to_rgb8();
    let buf = webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height()).encode_lossless();
    let tmp = out.with_extension("webp.tmp");
    std::fs::write(&tmp, &*buf).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    std::fs::rename(&tmp, out).map_err(|e| format!("cannot write {}: {e}", out.display()))
}

/// `doctest shrink`: re-encodes every screenshot under guide/img as lossless WebP (same pixels, wider than max_width
/// ones scaled down), removes the PNGs it replaced and points the guide sources at the new files.
pub struct Shrunk {
    pub files: usize,
    pub before: u64,
    pub after: u64,
    pub guides: usize,
}

pub fn shrink_all(cfg: &Config) -> Result<Shrunk, String> {
    fn walk(dir: &Path, ext: &[&str], out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, ext, out);
            } else if p.extension().is_some_and(|x| ext.iter().any(|e| x == *e)) {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(&cfg.out.join("guide/img"), &["png", "webp"], &mut files);
    let (mut before, mut after) = (0u64, 0u64);
    for f in &files {
        let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        before += size;
        let webp = f.with_extension("webp");
        let is_png = f.extension().is_some_and(|x| x == "png");
        if is_png && webp.is_file() {
            // a newer WebP of the same shot is already there
            std::fs::remove_file(f).map_err(|e| e.to_string())?;
            continue;
        }
        let mut img = image::open(f).map_err(|e| format!("{}: {e}", f.display()))?;
        let max = cfg.screenshots.max_width;
        if img.width() > max {
            let h = (img.height() as u64 * max as u64 / img.width() as u64) as u32;
            img = img.resize(max, h.max(1), image::imageops::FilterType::Lanczos3);
        }
        let tmp = f.with_extension("shrunk");
        write_image(&img, &tmp)?;
        let new = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(u64::MAX);
        if is_png || new < size {
            std::fs::rename(&tmp, &webp).map_err(|e| e.to_string())?;
            if is_png {
                std::fs::remove_file(f).map_err(|e| e.to_string())?;
            }
            after += new;
        } else {
            let _ = std::fs::remove_file(&tmp);
            after += size;
        }
    }
    let png_ref = regex::Regex::new(r"(guide/img/[^)\s]+)\.png\)").unwrap();
    let mut sources = Vec::new();
    walk(&crate::source::source_root(cfg), &["md"], &mut sources);
    let mut guides = 0;
    for p in sources {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let new = png_ref.replace_all(&text, |c: &regex::Captures| {
            if cfg.out.join(format!("{}.webp", &c[1])).is_file() {
                format!("{}.webp)", &c[1])
            } else {
                c[0].to_string()
            }
        });
        if new != text {
            std::fs::write(&p, new.as_bytes()).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
            guides += 1;
        }
    }
    Ok(Shrunk {
        files: files.len(),
        before,
        after,
        guides,
    })
}

/// `doctor`: opens the app's first page in headless Chrome, as screenshots will. Returns the URL it ended on and
/// how long it took.
pub fn probe(cfg: &Config) -> Result<(String, f64), String> {
    let chrome = chrome_path().ok_or("Chrome not found; install Google Chrome or set CHROME_PATH")?;
    let started = std::time::Instant::now();
    let (_browser, tab) = launch(&chrome, true, None, CALL_TIMEOUT)?;
    let (login, _) = cfg.login_for(&default_user(cfg));
    let url = format!(
        "{}{}",
        cfg.app.base_url,
        if login.is_empty() { "/" } else { login.as_str() }
    );
    tab.navigate_to(&url)
        .and_then(|t| t.wait_until_navigated())
        .map_err(|e| format!("cannot open {url}: {e}"))?;
    let status = tab
        .evaluate("document.body ? document.body.innerText.length : -1", false)
        .ok()
        .and_then(|v| v.value)
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    if status < 0 {
        return Err(format!("{url} opened but has no page body"));
    }
    Ok((tab.get_url(), started.elapsed().as_secs_f64()))
}

pub struct Shooter {
    cfg: Config,
    chrome: PathBuf,
    sessions: BTreeMap<String, Session>,
    current: Option<String>,
    /// When the shot being taken must stop (SHOT_LIMIT after it started).
    deadline: Option<std::time::Instant>,
}

fn mask_js(masks: &[Mask]) -> String {
    let list: Vec<serde_json::Value> = masks
        .iter()
        .map(|m| match m {
            Mask::Selector { selector, text } => serde_json::json!({"selector": selector, "text": text}),
            Mask::Regex { regex, text } => {
                let (flags, re) = match regex.strip_prefix("(?i)") {
                    Some(r) => ("gi", r),
                    None => ("g", regex.as_str()),
                };
                serde_json::json!({"regex": re, "flags": flags, "text": text})
            }
            Mask::Hide { hide } => serde_json::json!({"hide": hide}),
        })
        .collect();
    // Applies the masks now and again whenever the page changes (data arriving late, a re-render), and returns how
    // many places each mask changed.
    format!(
        r#"(function(masks){{
  function all(root, sel, out) {{
    out = out || [];
    try {{ root.querySelectorAll(sel).forEach(function(e){{ out.push(e); }}); }} catch (x) {{}}
    root.querySelectorAll('*').forEach(function(e){{ if (e.shadowRoot) all(e.shadowRoot, sel, out); }});
    return out;
  }}
  function texts(root, out) {{
    out = out || [];
    var w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT); var n;
    while ((n = w.nextNode())) out.push(n);
    root.querySelectorAll('*').forEach(function(e){{ if (e.shadowRoot) texts(e.shadowRoot, out); }});
    return out;
  }}
  function apply() {{
    return masks.map(function(m){{
      var hits = 0;
      if (m.hide) {{
        all(document, m.hide).forEach(function(e){{ hits++; if (e.style.display !== 'none') e.style.setProperty('display', 'none', 'important'); }});
      }} else if (m.selector) {{
        all(document, m.selector).forEach(function(e){{
          hits++;
          if ('value' in e && (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA')) {{ if (e.value !== m.text) {{ e.value = m.text; e.setAttribute('value', m.text); }} }}
          else if (e.textContent !== m.text) {{ e.textContent = m.text; }}
        }});
      }} else if (m.regex) {{
        var re; try {{ re = new RegExp(m.regex, m.flags); }} catch (x) {{ return -1; }}
        texts(document.body).forEach(function(n){{
          re.lastIndex = 0;
          if (re.test(n.nodeValue)) {{ re.lastIndex = 0; var v = n.nodeValue.replace(re, m.text); if (v !== n.nodeValue) {{ n.nodeValue = v; hits++; }} }}
        }});
        all(document, 'input,textarea').forEach(function(e){{
          re.lastIndex = 0;
          if (re.test(e.value)) {{ re.lastIndex = 0; e.value = e.value.replace(re, m.text); hits++; }}
        }});
      }}
      return hits;
    }});
  }}
  if (window.__doctestObserver) window.__doctestObserver.disconnect();
  var counts = apply();
  var pending = false;
  var obs = new MutationObserver(function(){{
    if (pending) return; pending = true;
    requestAnimationFrame(function(){{ pending = false; obs.disconnect(); apply(); obs.observe(document.body, {{subtree: true, childList: true, characterData: true}}); }});
  }});
  obs.observe(document.body, {{subtree: true, childList: true, characterData: true}});
  window.__doctestObserver = obs;
  return JSON.stringify(counts);
}})({})"#,
        serde_json::Value::Array(list)
    )
}

fn mask_name(m: &Mask) -> String {
    match m {
        Mask::Selector { selector, .. } => format!("selector {selector:?}"),
        Mask::Regex { regex, .. } => format!("regex {regex:?}"),
        Mask::Hide { hide } => format!("hide {hide:?}"),
    }
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
            deadline: None,
        })
    }

    fn creds(&self, user: &str) -> Result<(String, String), String> {
        let name = self.cfg.creds_user(user).unwrap_or_else(|| user.to_string());
        let Some((_, c)) = self.cfg.app.users.iter().find(|(n, _)| *n == name) else {
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
                // clear what is there (select it all and delete, so frameworks see the change), then type
                tab.evaluate(
                    &format!(
                        "(function(){{var e=document.querySelector({});if(!e)return;e.focus();if(e.select)e.select();else if(e.isContentEditable){{var r=document.createRange();r.selectNodeContents(e);var s=getSelection();s.removeAllRanges();s.addRange(r);}}}})()",
                        serde_json::to_string(fill).unwrap_or_default()
                    ),
                    false,
                )
                .map_err(|e| format!("step {i}: {e}"))?;
                tab.press_key("Backspace").map_err(|e| format!("step {i}: {e}"))?;
                tab.evaluate(
                    &format!(
                        "(function(){{var e=document.querySelector({});if(e&&'value' in e&&e.value!==''){{e.value='';e.dispatchEvent(new Event('input',{{bubbles:true}}));}}}})()",
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
            Step::WaitText { wait_text } => {
                let want = subst(wait_text);
                let js = format!(
                    "document.body.innerText.indexOf({}) >= 0",
                    serde_json::to_string(&want).unwrap_or_default()
                );
                let end = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let seen = tab
                        .evaluate(&js, false)
                        .ok()
                        .and_then(|v| v.value)
                        .and_then(|v| v.as_bool());
                    if seen == Some(true) {
                        break;
                    }
                    if std::time::Instant::now() > end {
                        return Err(format!(
                            "step {i}: the text {want:?} did not appear on {path} within 10 s"
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Step::Scroll { scroll } => match scroll.as_str() {
                "top" | "bottom" => {
                    let y = if scroll == "top" {
                        "0"
                    } else {
                        "document.body.scrollHeight"
                    };
                    tab.evaluate(&format!("window.scrollTo(0, {y})"), false)
                        .map_err(|e| format!("step {i}: {e}"))?;
                    std::thread::sleep(Duration::from_millis(300));
                }
                sel => {
                    find(sel)?.scroll_into_view().map_err(|e| format!("step {i}: {e}"))?;
                    std::thread::sleep(Duration::from_millis(300));
                }
            },
            Step::Goto { goto } => {
                open(tab, &format!("{base}{}", subst(goto))).map_err(|e| format!("step {i}: {e}"))?;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
        Ok(())
    }

    fn session(&mut self, user: Option<&str>, lang: &str) -> Result<Arc<Tab>, String> {
        let user = user.map(String::from).unwrap_or_else(|| default_user(&self.cfg));
        let base = self.cfg.app.base_url.clone();
        if !self.sessions.contains_key(&user) {
            let (login, login_steps) = self.cfg.login_for(&user);
            // the session `doctest login` saved, in a private copy; else sign in with the configured env vars
            let saved = signed_in(&self.cfg, &user);
            let copy = if saved {
                let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
                copy_profile(&profile_dir(&self.cfg, &user), dir.path())
                    .map_err(|e| format!("cannot copy the saved session: {e}"))?;
                Some(dir)
            } else {
                None
            };
            let (browser, tab) = launch(&self.chrome, true, copy.as_ref().map(|d| d.path()), CALL_TIMEOUT)?;
            let has_creds = self.cfg.creds_user(&user).is_some();
            if !saved && !login.is_empty() && !has_creds {
                return Err(format!(
                    "not signed in as {user}: run `doctest login{}` once — a Chrome window opens, you sign in by hand (OTP is fine), and screenshots reuse that session",
                    if user == "default" {
                        String::new()
                    } else {
                        format!(" --user {user}")
                    }
                ));
            }
            if !saved && !login.is_empty() {
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
            // safe mode: watch data and page requests; the guard refuses saves while a screenshot is taken
            let guard = Arc::new(Guard {
                allow: self.cfg.screenshots.allow.clone(),
                ..Default::default()
            });
            // one colour scheme for every guide image, whatever the machine or a restarted Chrome prefers
            let scheme = self.cfg.screenshots.color_scheme.to_lowercase();
            if scheme == "light" || scheme == "dark" {
                use headless_chrome::protocol::cdp::Emulation;
                tab.call_method(Emulation::SetEmulatedMedia {
                    media: None,
                    features: Some(vec![Emulation::MediaFeature {
                        name: "prefers-color-scheme".into(),
                        value: scheme,
                    }]),
                })
                .map_err(|e| format!("cannot set the colour scheme: {e}"))?;
            }
            // a dialog (alert, confirm, prompt, "Leave site?") stops the page until it is answered: answer it at once
            // (alerts and "Leave site?" with OK, confirm and prompt with Cancel, so nothing is confirmed) and say so
            let dialog = tab.get_dialog();
            let g = guard.clone();
            tab.add_event_listener(Arc::new(move |event: &headless_chrome::protocol::cdp::types::Event| {
                use headless_chrome::protocol::cdp::Page::DialogType;
                if let headless_chrome::protocol::cdp::types::Event::PageJavascriptDialogOpening(e) = event {
                    let (kind, accept) = match e.params.Type {
                        DialogType::Alert => ("alert", true),
                        DialogType::Beforeunload => ("leave-page prompt", true),
                        DialogType::Confirm => ("confirm", false),
                        DialogType::Prompt => ("prompt", false),
                    };
                    g.dialogs.lock().expect("guard").push(format!(
                        "{kind} {:?} ({})",
                        e.params.message,
                        if accept { "OK" } else { "Cancel" }
                    ));
                    let _ = if accept { dialog.accept(None) } else { dialog.dismiss() };
                }
            }))
            .map_err(|e| format!("cannot watch dialogs: {e}"))?;
            // what the servers refuse (401/419) explains a shot that ends on the sign-in page
            let g = guard.clone();
            tab.register_response_handling(
                "doctest-refused",
                Box::new(move |params, _body| {
                    let r = &params.response;
                    if r.status == 401 || r.status == 419 {
                        let url = r.url.split('?').next().unwrap_or_default();
                        g.refused.lock().expect("guard").push(format!("{} ({})", url, r.status));
                    }
                }),
            )
            .map_err(|e| format!("cannot watch responses: {e}"))?;
            if self.cfg.screenshots.safe {
                use headless_chrome::protocol::cdp::{Fetch, Network};
                let pattern = |t: Network::ResourceType| Fetch::RequestPattern {
                    url_pattern: Some("*".into()),
                    resource_Type: Some(t),
                    request_stage: Some(Fetch::RequestStage::Request),
                };
                tab.enable_fetch(
                    Some(&[
                        pattern(Network::ResourceType::Xhr),
                        pattern(Network::ResourceType::Fetch),
                        pattern(Network::ResourceType::Document),
                    ]),
                    None,
                )
                .map_err(|e| format!("cannot turn on safe mode: {e}"))?;
                let g = guard.clone();
                tab.enable_request_interception(Arc::new(
                    move |_t, _s, ev: headless_chrome::protocol::cdp::Fetch::events::RequestPausedEvent| {
                        let r = &ev.params.request;
                        if g.armed.load(Ordering::SeqCst) && !allowed(&r.method, &r.url, &g.allow) {
                            let path = r
                                .url
                                .split("://")
                                .nth(1)
                                .and_then(|x| x.split_once('/'))
                                .map(|(_, p)| p)
                                .unwrap_or(&r.url);
                            g.blocked.lock().expect("guard").push(format!(
                                "{} /{}",
                                r.method,
                                path.split('?').next().unwrap_or_default()
                            ));
                            headless_chrome::browser::tab::RequestPausedDecision::Fail(Fetch::FailRequest {
                                request_id: ev.params.request_id.clone(),
                                error_reason: Network::ErrorReason::BlockedByClient,
                            })
                        } else {
                            headless_chrome::browser::tab::RequestPausedDecision::Continue(None)
                        }
                    },
                ))
                .map_err(|e| format!("cannot turn on safe mode: {e}"))?;
            }
            self.sessions.insert(
                user.clone(),
                Session {
                    guard,
                    _browser: browser,
                    tab,
                    lang: None,
                    _profile: copy,
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

    /// Takes the screenshot of a spec and writes it (lossless WebP, mode 644, at most max_width wide). When Chrome went
    /// away (crashed, or its connection dropped), or the app dropped its sign-in in this browser and went to its
    /// sign-in page, it starts a new browser from the saved sign-in and tries once more.
    pub fn capture(&mut self, spec: &ShotSpec) -> Result<Shot, String> {
        match self.capture_once(spec) {
            // the app dropped its sign-in in this browser: the next shot (and this retry) starts from the saved one
            Err(e) if e.starts_with(SIGNED_OUT) => {
                let user = spec.user.clone().unwrap_or_else(|| default_user(&self.cfg));
                self.sessions.remove(&user);
                let r = self.capture_once(spec);
                if r.is_err() {
                    self.sessions.remove(&user);
                }
                r
            }
            Err(e) if dropped(&e) => {
                let user = spec.user.clone().unwrap_or_else(|| default_user(&self.cfg));
                self.sessions.remove(&user);
                self.capture_once(spec)
                    .map_err(|e2| format!("{e2} (Chrome was restarted after: {e})"))
            }
            r => r,
        }
    }

    fn capture_once(&mut self, spec: &ShotSpec) -> Result<Shot, String> {
        validate_spec(spec)?;
        let path = resolve_path(spec)?;
        let tab = self.session(spec.user.as_deref(), &spec.lang)?;
        let user_key = self.current.clone().unwrap_or_default();
        let guard = self.sessions[&user_key].guard.clone();
        guard.blocked.lock().expect("guard").clear();
        guard.refused.lock().expect("guard").clear();
        guard.dialogs.lock().expect("guard").clear();
        self.deadline = Some(std::time::Instant::now() + SHOT_LIMIT);
        guard
            .armed
            .store(self.cfg.screenshots.safe && !spec.allow_saves, Ordering::SeqCst);
        let r = self.shoot_page(&tab, spec, &path);
        guard.armed.store(false, Ordering::SeqCst);
        let blocked = std::mem::take(&mut *guard.blocked.lock().expect("guard"));
        let dialogs = std::mem::take(&mut *guard.dialogs.lock().expect("guard"));
        self.deadline = None;
        let (out, unmatched_masks) = r.map_err(|e| {
            if dialogs.is_empty() {
                e
            } else {
                format!("{e} (the page opened: {})", dialogs.join("; "))
            }
        })?;
        Ok(Shot {
            path: out,
            blocked,
            unmatched_masks,
            dialogs,
        })
    }

    fn shoot_page(&mut self, tab: &Arc<Tab>, spec: &ShotSpec, path: &str) -> Result<(PathBuf, Vec<String>), String> {
        let tab = tab.clone();
        let path = path.to_string();
        let base = self.cfg.app.base_url.clone();
        open(&tab, &format!("{base}{path}"))?;
        let deadline = self.deadline.unwrap_or_else(|| std::time::Instant::now() + SHOT_LIMIT);
        let left = |max: Duration| max.min(deadline.saturating_duration_since(std::time::Instant::now()));
        settle(&tab, left(Duration::from_secs(8)));
        let user = self.current.clone().unwrap_or_default();
        let (login_url, _) = self.cfg.login_for(&user);
        // the sign-in page of any portal (an app may send a selfcare page to the admin sign-in)
        let mut logins: Vec<String> = self.cfg.login_targets().into_iter().map(|(_, l)| l).collect();
        logins.push(login_url.clone());
        logins.push(self.cfg.app.login_url.clone());
        logins.retain(|l| !l.is_empty());
        let shooting_a_login = logins.iter().any(|l| on_login_page(&format!("x://h{path}"), l));
        let guard = self.sessions.get(&user).map(|s| s.guard.clone());
        let signed_out = |tab: &Arc<Tab>| -> Result<(), String> {
            let url = tab.get_url();
            if !shooting_a_login && logins.iter().any(|l| on_login_page(&url, l)) {
                let (refused, blocked) = guard
                    .as_ref()
                    .map(|g| {
                        (
                            g.refused.lock().expect("guard").clone(),
                            g.blocked.lock().expect("guard").clone(),
                        )
                    })
                    .unwrap_or_default();
                return Err(signed_out_error(&user, &refused, &blocked));
            }
            Ok(())
        };
        signed_out(&tab)?;
        let lang = spec.lang.clone();
        let subst = move |v: &str| v.replace("$lang", &lang);
        for (i, step) in spec.steps.iter().enumerate() {
            if std::time::Instant::now() > deadline {
                return Err(format!(
                    "the shot of {path} took over {} s and stopped before step {}: the page did not finish or a step \
                     waits for something that never comes",
                    SHOT_LIMIT.as_secs(),
                    i + 1
                ));
            }
            Self::run_step(&tab, i + 1, step, &path, &subst, &base)?;
        }
        settle(&tab, left(Duration::from_secs(5)));
        signed_out(&tab)?;
        let masks: Vec<Mask> = self.cfg.screenshots.mask.iter().chain(&spec.mask).cloned().collect();
        let counts = tab
            .evaluate(&mask_js(&masks), false)
            .map_err(|e| format!("masking failed: {e}"))?
            .value
            .and_then(|v| v.as_str().and_then(|s| serde_json::from_str::<Vec<i64>>(s).ok()))
            .unwrap_or_default();
        let unmatched: Vec<String> = masks
            .iter()
            .zip(counts.iter().chain(std::iter::repeat(&0)))
            .filter(|(_, n)| **n <= 0)
            .map(|(m, n)| {
                if *n < 0 {
                    format!("{} (not a valid JavaScript regular expression)", mask_name(m))
                } else {
                    mask_name(m)
                }
            })
            .collect();
        std::thread::sleep(Duration::from_millis(250));
        let png = match &spec.crop {
            Some(sel) => {
                tab.wait_for_element_with_custom_timeout(sel, Duration::from_secs(10))
                    .map_err(|_| format!("crop selector {sel:?} not found on {path}"))?;
                // below the first screen, also inside a layout's own scroll box: bring it into view, then cut its box
                // out of what the screen shows (Chrome's element clip uses document coordinates and comes out blank
                // for an element inside a scrolled container)
                let js = format!(
                    "(function(){{var e=document.querySelector({});if(!e)return null;e.scrollIntoView({{block:'center',inline:'nearest'}});return null;}})()",
                    serde_json::to_string(sel).unwrap_or_default()
                );
                tab.evaluate(&js, false)
                    .map_err(|e| format!("cannot scroll to {sel:?}: {e}"))?;
                std::thread::sleep(Duration::from_millis(500));
                let rect_js = format!(
                    "(function(){{var r=document.querySelector({}).getBoundingClientRect();return JSON.stringify([r.left,r.top,r.width,r.height,window.devicePixelRatio||1]);}})()",
                    serde_json::to_string(sel).unwrap_or_default()
                );
                let rect: Vec<f64> = tab
                    .evaluate(&rect_js, false)
                    .ok()
                    .and_then(|v| v.value)
                    .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
                    .ok_or_else(|| format!("cannot measure {sel:?}"))?;
                let shot = tab
                    .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
                    .map_err(|e| format!("screenshot failed: {e}"))?;
                crop_png(&shot, &rect).map_err(|e| format!("crop {sel:?}: {e}"))?
            }
            None => tab
                .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
                .map_err(|e| format!("screenshot failed: {e}"))?,
        };
        let mut img = image::load_from_memory(&png).map_err(|e| format!("bad screenshot: {e}"))?;
        if is_blank(&img) {
            return Err(format!(
                "the screenshot of {path}{} is blank (one colour): the content had not drawn yet or is outside the \
                 visible area",
                spec.crop
                    .as_ref()
                    .map(|c| format!(" cropped to {c:?}"))
                    .unwrap_or_default()
            ));
        }
        let max = self.cfg.screenshots.max_width;
        if img.width() > max {
            let h = (img.height() as u64 * max as u64 / img.width() as u64) as u32;
            img = img.resize(max, h.max(1), image::imageops::FilterType::Lanczos3);
        }
        let out = image_path(&self.cfg, spec);
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        write_image(&img, &out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
        }
        Ok((out, unmatched))
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
pub(crate) mod tests {
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
            cfg.out.join("guide/img/id/orders/shop-orders-create-1.webp")
        );
        assert_eq!(image_ref(&s), "guide/img/id/orders/shop-orders-create-1.webp");
    }

    #[test]
    fn spec_id_must_be_safe() {
        let mut s = spec("/x");
        s.id = "../etc/passwd".into();
        assert!(validate_spec(&s).unwrap_err().contains("id"));
    }

    #[test]
    fn profile_folder_per_project_and_user() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_in(d.path(), "");
        let a = profile_dir(&cfg, "admin");
        let b = profile_dir(&cfg, "tenant");
        assert_ne!(a, b);
        assert!(a.starts_with(crate::reports::tool_root().join("chrome")));
        assert!(a.ends_with("admin"));
        assert!(!signed_in(&cfg, "nobody-here"));
    }

    #[test]
    fn profile_copy_skips_locks_and_caches() {
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("p");
        std::fs::create_dir_all(from.join("Default/Cache")).unwrap();
        std::fs::create_dir_all(from.join("Default/Local Storage")).unwrap();
        std::fs::write(from.join("Default/Cookies"), "c").unwrap();
        std::fs::write(from.join("Default/Local Storage/x.ldb"), "l").unwrap();
        std::fs::write(from.join("Default/Cache/big"), "x").unwrap();
        std::fs::write(from.join("SingletonLock"), "").unwrap();
        let to = d.path().join("copy");
        copy_profile(&from, &to).unwrap();
        assert!(to.join("Default/Cookies").is_file() && to.join("Default/Local Storage/x.ldb").is_file());
        assert!(!to.join("Default/Cache").exists() && !to.join("SingletonLock").exists());
    }

    #[test]
    fn user_name_defaults_to_default() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_in(d.path(), "");
        assert_eq!(default_user(&cfg), "default");
    }

    /// Serves tests/fixtures/static-app on 127.0.0.1:<port>.
    pub(crate) fn serve() -> u16 {
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
                    let status = if path.starts_with("api/denied") {
                        "401 Unauthorized"
                    } else {
                        "200 OK"
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    fn shrink_makes_screenshots_smaller() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_in(d.path(), "[screenshots]\nmax_width=100\n");
        let dir = cfg.out.join("guide/img/en/m");
        std::fs::create_dir_all(&dir).unwrap();
        let img = image::RgbaImage::from_fn(300, 120, |x, y| {
            image::Rgba([(x % 7 * 30) as u8, (y % 5 * 40) as u8, 200, 255])
        });
        let f = dir.join("p-1.png");
        img.save_with_format(&f, image::ImageFormat::Png).unwrap();
        let src = crate::source::source_root(&cfg).join("guide/en/m");
        std::fs::create_dir_all(&src).unwrap();
        let guide = src.join("p.md");
        std::fs::write(&guide, "![a](guide/img/en/m/p-1.png) ![b](guide/img/en/m/gone-2.png)\n").unwrap();
        let r = shrink_all(&cfg).unwrap();
        assert_eq!((r.files, r.guides), (1, 1));
        assert!(r.after < r.before, "{} -> {}", r.before, r.after);
        assert!(!f.exists(), "the PNG is replaced");
        let back = image::open(dir.join("p-1.webp")).unwrap();
        assert_eq!(back.width(), 100);
        assert!(back.color() == image::ColorType::Rgb8);
        assert_eq!(
            std::fs::read_to_string(&guide).unwrap(),
            "![a](guide/img/en/m/p-1.webp) ![b](guide/img/en/m/gone-2.png)\n",
            "only references whose WebP exists change"
        );
    }

    #[test]
    fn screenshots_are_lossless_webp() {
        let d = tempfile::tempdir().unwrap();
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(64, 40, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 6) as u8, ((x + y) % 256) as u8])
        }));
        let out = d.path().join("s.webp");
        write_image(&img, &out).unwrap();
        assert_eq!(&std::fs::read(&out).unwrap()[8..12], b"WEBP");
        assert_eq!(image::open(&out).unwrap().to_rgb8(), img.to_rgb8(), "same pixels");
    }

    #[test]
    fn a_shot_on_the_sign_in_page_says_why() {
        let e = signed_out_error("admin", &["http://x/api/selfcare/me (401)".into()], &[]);
        assert!(e.starts_with(SIGNED_OUT) && e.contains("api/selfcare/me (401)") && e.contains("do not stop the run"));
        assert!(
            !e.contains("expired"),
            "a refused request is not an expired session: {e}"
        );
        let e = signed_out_error("admin", &[], &["POST /api/token/refresh".into()]);
        assert!(e.contains("safe mode refused POST /api/token/refresh") && e.contains("[screenshots] allow"));
        let e = signed_out_error("admin", &[], &[]);
        assert!(e.contains("may have expired") && e.contains("doctest login --user admin"));
    }

    #[test]
    fn safe_mode_allows_reads_and_listed_requests() {
        let allow = vec!["POST /api/*/search".to_string(), "/api/login".to_string()];
        assert!(allowed("GET", "http://x/api/orders", &[]));
        assert!(!allowed("POST", "http://x/api/orders", &allow));
        assert!(!allowed("delete", "http://x/api/orders/1", &allow));
        assert!(allowed("POST", "http://x/api/orders/search?q=1", &allow));
        assert!(!allowed("PUT", "http://x/api/orders/search", &allow));
        assert!(allowed("PUT", "http://x/api/login", &allow));
    }

    #[test]
    fn new_steps_and_masks_parse() {
        let steps: Vec<Step> = serde_json::from_str(
            r##"[{"wait_text":"Saved"},{"scroll":"bottom"},{"wait":".x"},{"fill":"#a","value":"b"}]"##,
        )
        .unwrap();
        assert!(matches!(steps[0], Step::WaitText { .. }));
        assert!(matches!(steps[1], Step::Scroll { .. }));
        assert!(matches!(steps[2], Step::Wait { .. }));
        let masks: Vec<Mask> = serde_json::from_str(r#"[{"hide":".x"},{"regex":"(?i)ab","text":"c"}]"#).unwrap();
        assert!(matches!(masks[0], Mask::Hide { .. }));
        let js = mask_js(&masks);
        assert!(js.contains(r#""flags":"gi""#) && js.contains(r#""regex":"ab""#), "{js}");
    }

    #[test]
    fn finds_doctest_chromes_by_owner() {
        assert!(OWNER_ARG.starts_with("--doctest-owner="));
        assert!(dropped(
            "Unable to make method calls because underlying connection is closed"
        ));
        assert!(!dropped("selector \"#x\" not found"));
        // our own process is alive, so nothing of ours is an orphan
        assert!(alive(std::process::id()));
    }

    #[test]
    #[ignore = "needs Chrome"]
    fn safe_mode_blocks_saves_and_masks_late_data() {
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        let mut cfg = live_cfg(d.path(), port);
        cfg.screenshots.allow = vec!["POST /api/*/search".into()];
        cfg.screenshots.mask.push(Mask::Hide { hide: ".secret".into() });
        let mut sh = Shooter::start(&cfg).unwrap();
        let mut s = spec("/save.html");
        s.steps = vec![
            Step::Fill {
                fill: "#name".into(),
                value: "new".into(),
            },
            Step::Click { click: "#save".into() },
            Step::WaitText {
                wait_text: "Refused".into(),
            },
            Step::Scroll {
                scroll: "bottom".into(),
            },
        ];
        let shot = sh.capture(&s).unwrap();
        assert_eq!(shot.blocked, vec!["POST /api/orders"]);
        assert_eq!(sh.input_value("#name").unwrap(), "new");
        let text = sh.page_text().unwrap();
        assert!(!text.contains("Secret panel"), "{text}");
        assert!(text.contains("D-01-01") && !text.contains("AB-99-99"), "{text}");
        // the late text is masked once it arrives
        std::thread::sleep(Duration::from_millis(900));
        let text = sh.page_text().unwrap();
        assert!(!text.contains("CD-11-22"), "{text}");
        // a listed read is let through; Chrome's own process is tagged with the owner
        let mut s2 = spec("/save.html");
        s2.steps = vec![Step::Click {
            click: "#search".into(),
        }];
        let shot = sh.capture(&s2).unwrap();
        assert!(shot.blocked.is_empty(), "{:?}", shot.blocked);
        assert!(doctest_chromes().iter().any(|(_, o)| *o == std::process::id()));
        drop(sh);
        let n = doctest_chromes()
            .iter()
            .filter(|(_, o)| *o == std::process::id())
            .count();
        assert_eq!(n, 0, "Chrome left running after the shooter closed");
    }

    #[test]
    #[ignore = "needs Chrome"]
    fn dialogs_are_answered_crops_work_in_scroll_boxes_and_blank_shots_are_refused() {
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        let cfg = live_cfg(d.path(), port);
        let mut sh = Shooter::start(&cfg).unwrap();
        // an alert on load and a confirm from a step used to stop the page until Chrome gave up
        let mut s = spec("/dialog.html");
        s.steps = vec![crate::config::Step::Click { click: "#go".into() }];
        let started = std::time::Instant::now();
        let shot = sh.capture(&s).unwrap();
        assert!(started.elapsed() < Duration::from_secs(40), "{:?}", started.elapsed());
        assert!(
            shot.dialogs
                .iter()
                .any(|d| d.contains("Welcome back") && d.contains("OK")),
            "{:?}",
            shot.dialogs
        );
        assert!(
            shot.dialogs
                .iter()
                .any(|d| d.contains("Delete it?") && d.contains("Cancel")),
            "{:?}",
            shot.dialogs
        );
        // every guide image in the same colour scheme (light unless [screenshots] color_scheme says otherwise)
        let light = sh
            .current_tab()
            .unwrap()
            .evaluate("matchMedia('(prefers-color-scheme: light)').matches", false)
            .unwrap()
            .value;
        assert_eq!(light, Some(serde_json::json!(true)));
        // a section below the fold of a layout's own scroll box
        let mut s = spec("/scrollbox.html");
        s.id = "orders/low-1".into();
        s.crop = Some("#low".into());
        let out = sh.capture(&s).unwrap().path;
        let img = image::open(&out).unwrap().to_rgb8();
        let mid = img.get_pixel(img.width() / 4, img.height() / 2);
        assert!(mid[0] > 150 && mid[1] < 80, "the red section, not a blank: {mid:?}");
        // a page that drew nothing
        let mut s = spec("/blank.html");
        s.id = "orders/blank-1".into();
        let e = sh.capture(&s).unwrap_err();
        assert!(e.contains("blank"), "{e}");
    }

    #[test]
    #[ignore = "needs Chrome"]
    fn a_page_that_drops_the_sign_in_does_not_break_later_shots() {
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        let cfg = live_cfg(d.path(), port);
        let mut sh = Shooter::start(&cfg).unwrap();
        sh.capture(&spec("/orders.html")).unwrap();
        let e = sh.capture(&spec("/denied.html")).unwrap_err();
        assert!(e.starts_with(SIGNED_OUT) && e.contains("/api/denied (401)"), "{e}");
        assert!(e.contains("do not stop the run"), "{e}");
        // the next shot starts from the sign-in again instead of the browser the app signed out
        sh.capture(&spec("/orders.html")).unwrap();
        let text = sh.page_text().unwrap();
        assert!(text.contains("demo") && !text.contains("nobody"), "{text}");
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
        let out = sh.capture(&s).unwrap().path;
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
    fn login_once_then_screenshots_reuse_the_session() {
        let port = serve();
        let d = tempfile::tempdir().unwrap();
        // DOCTEST_HOME keeps the test profile out of ~/.doctest; only this ignored test sets it
        unsafe { std::env::set_var("DOCTEST_HOME", d.path().join("home")) };
        let cfg = cfg_in(
            d.path(),
            &format!("[app]\nbase_url=\"http://127.0.0.1:{port}\"\nlogin_url=\"/login.html\"\n"),
        );
        // the person signs in by hand; the test does it through the same window
        let user = login(&cfg, "default", false, &|tab: &Tab| {
            tab.wait_for_element("[name=username]").unwrap().click().unwrap();
            tab.type_str("handmade").unwrap();
            tab.wait_for_element("button[type=submit]").unwrap().click().unwrap();
        })
        .unwrap();
        assert_eq!(user, "default");
        assert!(signed_in(&cfg, "default"));
        let mut sh = Shooter::start(&cfg).unwrap();
        sh.capture(&spec("/orders.html")).unwrap();
        assert!(
            sh.page_text().unwrap().contains("handmade"),
            "the saved session (local storage) is reused"
        );
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
        let img = image::open(sh.capture(&s).unwrap().path).unwrap();
        assert!(img.height() < 200, "{}", img.height());
    }
}
