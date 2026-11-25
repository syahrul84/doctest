//! Code facts from codemap: one `codemap serve` child per app, spoken to over MCP stdio.
//! Tests use `FixtureCodemap`, which answers from JSON files.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

use crate::config::Config;

/// The app name for the backend; a frontend's app name is its folder name.
pub const BACKEND: &str = "backend";

#[derive(Debug, Clone)]
pub struct Answer {
    pub data: Value,
    pub stale: bool,
    /// For report_wrong_answer when an answer turns out wrong.
    #[allow(dead_code)]
    pub call_id: Option<String>,
    pub next_cursor: Option<String>,
}

pub trait CodemapApi {
    fn call(&mut self, app: &str, tool: &str, args: Value) -> Result<Answer, String>;

    /// Every item of a paged list (`list_key` inside `data`), following `next_cursor`.
    fn paged(&mut self, app: &str, tool: &str, args: Value, list_key: &str) -> Result<(Vec<Value>, bool), String> {
        let mut items = Vec::new();
        let mut stale = false;
        let mut cursor: Option<String> = None;
        loop {
            let mut a = args.clone();
            a["limit"] = json!(500);
            if let Some(c) = &cursor {
                a["cursor"] = json!(c);
            }
            let answer = self.call(app, tool, a)?;
            stale |= answer.stale;
            if let Some(list) = answer.data[list_key].as_array() {
                items.extend(list.iter().cloned());
            }
            match answer.next_cursor {
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => return Ok((items, stale)),
            }
        }
    }
}

/// codemap's tool result text → Answer.
pub fn parse_answer(text: &str) -> Result<Answer, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("codemap answer is not JSON: {e}"))?;
    let meta = &v["meta"];
    Ok(Answer {
        data: v.get("data").cloned().unwrap_or(Value::Null),
        stale: meta["stale"].as_bool().unwrap_or(false),
        call_id: meta["call_id"].as_str().map(String::from),
        next_cursor: meta["next_cursor"].as_str().map(String::from),
    })
}

/// CODEMAP_BIN, else codemap on the PATH, else ~/.local/bin/codemap.
pub fn binary() -> PathBuf {
    if let Ok(bin) = std::env::var("CODEMAP_BIN") {
        return PathBuf::from(bin);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("codemap");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/bin/codemap")
}

/// A failed write to codemap's stdin: it has exited (broken pipe) or cannot take input.
fn write_error(method: &str, e: &std::io::Error, stderr_tail: &str) -> String {
    format!("codemap stopped while sending {method} ({e}){stderr_tail}")
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
    stderr: tempfile::NamedTempFile,
    next_id: u64,
    timeout: std::time::Duration,
}

impl Server {
    fn spawn(bin: &Path, project: &Path, extra: &[String], timeout: std::time::Duration) -> Result<Server, String> {
        if !bin.is_file() {
            return Err(format!(
                "codemap not found at {}; install it or set CODEMAP_BIN",
                bin.display()
            ));
        }
        let stderr = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let err_file = stderr.reopen().map_err(|e| e.to_string())?;
        let mut child = Command::new(bin)
            .args(["serve", "--project"])
            .arg(project)
            .args(["--log-level", "warn"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err_file))
            .spawn()
            .map_err(|e| format!("cannot start codemap ({}): {e}", bin.display()))?;
        let stdin = child.stdin.take().ok_or("codemap: no stdin")?;
        let stdout: ChildStdout = child.stdout.take().ok_or("codemap: no stdout")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut s = Server {
            child,
            stdin,
            lines,
            stderr,
            next_id: 0,
            timeout,
        };
        s.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "doctest", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(s)
    }

    fn stderr_tail(&self) -> String {
        let text = std::fs::read_to_string(self.stderr.path()).unwrap_or_default();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(5)..].join(" | ");
        if tail.is_empty() {
            String::new()
        } else {
            format!(" (codemap said: {tail})")
        }
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let method = message["method"].as_str().unwrap_or("a request").to_string();
        writeln!(self.stdin, "{message}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| {
                // codemap exited before reading: give it a moment to finish writing why
                std::thread::sleep(std::time::Duration::from_millis(50));
                write_error(&method, &e, &self.stderr_tail())
            })
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = std::time::Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(l) => l,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let _ = self.child.kill();
                    return Err(format!(
                        "codemap did not answer {method} within {}s{}",
                        self.timeout.as_secs(),
                        self.stderr_tail()
                    ));
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    return Err(format!(
                        "codemap stopped while answering {method}{}",
                        self.stderr_tail()
                    ));
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("codemap {method}: {error}"));
                }
                return Ok(message["result"].clone());
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// codemap servers for the backend and each frontend, started on first use.
pub struct LiveCodemap {
    /// How long one codemap answer may take (DOCTEST_CODEMAP_TIMEOUT seconds, default 600).
    pub timeout: std::time::Duration,
    /// tool → (calls, total time); printed to stderr on drop when DOCTEST_PROFILE is set.
    profile: BTreeMap<String, (u32, std::time::Duration)>,
    /// Parallel worker number; > 0 gives the worker its own codemap indexes (shared ones slow each other down).
    worker: usize,
    bin: PathBuf,
    backend: PathBuf,
    frontends: BTreeMap<String, PathBuf>,
    servers: BTreeMap<String, Server>,
    /// Folders whose changes do not change codemap's answers to doctest: its own output and the tests it writes.
    ignore: Vec<PathBuf>,
}

/// Answers kept between tool calls (shared by every worker thread) while the code is unchanged:
/// (backend, app, tool, args) → (code signature, answer).
static MEMO: std::sync::LazyLock<std::sync::Mutex<HashMap<String, (String, Answer)>>> =
    std::sync::LazyLock::new(Default::default);
/// Backend → the code signature and when it was taken (taken again after a few seconds).
type Sig = (std::time::Instant, Option<String>);
static SIGS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Sig>>> = std::sync::LazyLock::new(Default::default);

/// Memo entries added since the last save.
static MEMO_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn memo_file(backend: &str) -> PathBuf {
    let id = crate::hash::sha256(backend);
    crate::reports::tool_root()
        .join("cache")
        .join(format!("codemap-{}.json", &id[..12]))
}

/// The answers a previous doctest kept for this backend, when the code still has the same signature.
fn load_memo(backend: &str, sig: &str) {
    let Ok(text) = std::fs::read_to_string(memo_file(backend)) else {
        return;
    };
    let Ok(saved) = serde_json::from_str::<Vec<(String, String, Value, Option<String>)>>(&text) else {
        return;
    };
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    for (key, s, data, next_cursor) in saved {
        if s == sig {
            memo.entry(key).or_insert((
                s,
                Answer {
                    data,
                    stale: false,
                    call_id: None,
                    next_cursor,
                },
            ));
        }
    }
}

/// Keeps the answers on disk (per backend), so the next doctest process starts warm. Called after each tool call.
pub fn save_memo() {
    if !MEMO_DIRTY.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let sigs: HashMap<String, Option<String>> = SIGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, (_, s))| (k.clone(), s.clone()))
        .collect();
    let memo = MEMO.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for (backend, sig) in sigs {
        let Some(sig) = sig else { continue };
        let prefix = format!("{backend}\u{1}");
        let rows: Vec<(String, String, Value, Option<String>)> = memo
            .iter()
            .filter(|(k, (s, _))| k.starts_with(&prefix) && *s == sig)
            .map(|(k, (s, a))| (k.clone(), s.clone(), a.data.clone(), a.next_cursor.clone()))
            .collect();
        let path = memo_file(&backend);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(t) = serde_json::to_string(&rows) {
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            if std::fs::write(&tmp, t).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// What the code of these repos looks like now, cheaply: each one's commit, the files git sees changed or new, and
/// their sizes and times. None when a folder is not a git repo (then nothing is kept between calls).
fn code_signature(dirs: &[&PathBuf], ignore: &[PathBuf]) -> Option<String> {
    let mut parts = Vec::new();
    for d in dirs {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(d)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let head = git(&["rev-parse", "HEAD"])?;
        let status = git(&["status", "--porcelain", "--untracked-files=all"])?;
        parts.push(head);
        for line in status.lines() {
            let path = line.get(3..).unwrap_or_default();
            let path = path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"');
            if ignore.iter().any(|i| d.join(path).starts_with(i)) {
                continue;
            }
            let meta = std::fs::metadata(d.join(path)).ok();
            let stamp = meta
                .map(|m| {
                    let t = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|t| t.as_nanos())
                        .unwrap_or(0);
                    format!("{}:{t}", m.len())
                })
                .unwrap_or_else(|| "gone".into());
            parts.push(format!("{line}|{stamp}"));
        }
    }
    Some(crate::hash::sha256(&parts.join("\n")))
}

impl LiveCodemap {
    pub fn new(cfg: &Config) -> LiveCodemap {
        LiveCodemap::with_binary(cfg, binary())
    }

    pub fn with_binary(cfg: &Config, bin: PathBuf) -> LiveCodemap {
        LiveCodemap {
            timeout: std::time::Duration::from_secs(
                std::env::var("DOCTEST_CODEMAP_TIMEOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(600),
            ),
            profile: BTreeMap::new(),
            worker: 0,
            bin,
            backend: cfg.backend.clone(),
            frontends: cfg
                .frontends
                .iter()
                .map(|f| (Config::repo_name(f), f.clone()))
                .collect(),
            servers: BTreeMap::new(),
            ignore: vec![
                cfg.out.clone(),
                cfg.backend.join("tests"),
                cfg.backend.join("storage"),
                cfg.backend.join(".doctest"),
            ],
        }
    }

    /// The current code signature, taken at most every few seconds (one tool call makes many codemap calls).
    fn signature(&self) -> Option<String> {
        let id = self.backend.to_string_lossy().into_owned();
        let mut sigs = SIGS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, sig)) = sigs.get(&id)
            && at.elapsed() < std::time::Duration::from_secs(3)
        {
            return sig.clone();
        }
        let first = !sigs.contains_key(&id);
        let mut dirs: Vec<&PathBuf> = vec![&self.backend];
        dirs.extend(self.frontends.values());
        let sig = code_signature(&dirs, &self.ignore);
        let changed = sigs.get(&id).is_some_and(|(_, old)| *old != sig);
        sigs.insert(id.clone(), (std::time::Instant::now(), sig.clone()));
        drop(sigs);
        if let Some(sg) = &sig
            && (first || changed)
        {
            load_memo(&id, sg);
        }
        sig
    }

    /// A client for parallel worker `n` (1-based), with its own index copies.
    pub fn worker(cfg: &Config, n: usize) -> LiveCodemap {
        let mut c = LiveCodemap::new(cfg);
        c.worker = n;
        c
    }

    fn server(&mut self, app: &str) -> Result<&mut Server, String> {
        if let Some(s) = self.servers.get_mut(app)
            && matches!(s.child.try_wait(), Ok(Some(_)))
        {
            self.servers.remove(app);
        }
        if !self.servers.contains_key(app) {
            let server = if app == BACKEND {
                let extra: Vec<String> = if self.worker > 0 {
                    vec![
                        "--project-id".into(),
                        format!("{}-doctest-w{}", Config::repo_name(&self.backend), self.worker),
                    ]
                } else {
                    Vec::new()
                };
                Server::spawn(&self.bin, &self.backend, &extra, self.timeout)?
            } else {
                let path = self.frontends.get(app).ok_or_else(|| {
                    format!(
                        "unknown app {app}; apps: backend, {}",
                        self.frontends.keys().cloned().collect::<Vec<_>>().join(", ")
                    )
                })?;
                let extra = vec![
                    "--project-id".to_string(),
                    if self.worker > 0 {
                        format!("{app}-doctest-w{}", self.worker)
                    } else {
                        format!("{app}-doctest")
                    },
                    "--link".to_string(),
                    format!("backend={}", self.backend.display()),
                ];
                Server::spawn(&self.bin, path, &extra, self.timeout)?
            };
            self.servers.insert(app.to_string(), server);
        }
        Ok(self.servers.get_mut(app).expect("inserted above"))
    }
}

impl CodemapApi for LiveCodemap {
    fn call(&mut self, app: &str, tool: &str, args: Value) -> Result<Answer, String> {
        // the same question on unchanged code gets the same answer: kept between tool calls
        let key = format!("{}\u{1}{app}\u{1}{tool}\u{1}{args}", self.backend.display());
        let sig = if tool == "report_wrong_answer" {
            None
        } else {
            self.signature()
        };
        if let Some(sig) = &sig
            && let Some((s, a)) = MEMO.lock().unwrap_or_else(|e| e.into_inner()).get(&key)
            && s == sig
        {
            return Ok(a.clone());
        }
        let started = std::time::Instant::now();
        let result = self
            .server(app)?
            .rpc("tools/call", json!({"name": tool, "arguments": args}));
        let e = self.profile.entry(format!("{app}:{tool}")).or_default();
        e.0 += 1;
        e.1 += started.elapsed();
        if result.is_err() {
            // a dead or hung codemap is started again on the next call
            self.servers.remove(app);
        }
        let result = result?;
        let text: String = result["content"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect())
            .unwrap_or_default();
        if result["isError"].as_bool() == Some(true) {
            return Err(format!("codemap {tool}: {text}"));
        }
        let answer = parse_answer(&text).map_err(|e| format!("codemap {tool}: {e}"))?;
        if let Some(sig) = sig
            && !answer.stale
        {
            let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
            if memo.len() > 20_000 {
                memo.clear();
            }
            memo.insert(key, (sig, answer.clone()));
            MEMO_DIRTY.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(answer)
    }
}

impl Drop for LiveCodemap {
    fn drop(&mut self) {
        if std::env::var_os("DOCTEST_PROFILE").is_some() {
            for (k, (n, d)) in &self.profile {
                eprintln!("codemap {k}: {n} calls, {:.1}s", d.as_secs_f64());
            }
        }
    }
}

/// Answers from `<dir>/[<app>/]<tool>[__<key>].json`; key = args name, file, symbol or uri_contains with `/` → `_`.
/// A missing file answers `found: false`.
pub struct FixtureCodemap {
    dir: PathBuf,
    pub calls: Vec<(String, String, Value)>,
}

impl FixtureCodemap {
    pub fn new(dir: PathBuf) -> FixtureCodemap {
        FixtureCodemap { dir, calls: Vec::new() }
    }
}

impl CodemapApi for FixtureCodemap {
    fn call(&mut self, app: &str, tool: &str, args: Value) -> Result<Answer, String> {
        self.calls.push((app.into(), tool.into(), args.clone()));
        let key = ["name", "file", "symbol", "table", "uri_contains", "path_contains"]
            .iter()
            .find_map(|k| args[*k].as_str())
            .map(|k| k.replace(['/', '\\', ':', ' '], "_"));
        let mut candidates = Vec::new();
        for base in [self.dir.join(app), self.dir.clone()] {
            if let Some(k) = &key {
                candidates.push(base.join(format!("{tool}__{k}.json")));
            }
            candidates.push(base.join(format!("{tool}.json")));
        }
        for c in candidates {
            if let Ok(text) = std::fs::read_to_string(&c) {
                return parse_answer(&text);
            }
        }
        Ok(Answer {
            data: json!({"found": false}),
            stale: false,
            call_id: None,
            next_cursor: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codemap")
    }

    #[test]
    fn fixture_answers_by_tool_and_name() {
        let mut api = FixtureCodemap::new(fixtures());
        let a = api
            .call("web", "get_component", json!({"name": "ShopOrderForm"}))
            .unwrap();
        assert_eq!(a.data["name"], "ShopOrderForm");
        assert!(!a.stale);
    }

    #[test]
    fn fixture_missing_is_not_found() {
        let mut api = FixtureCodemap::new(fixtures());
        let a = api.call("web", "get_component", json!({"name": "Nope"})).unwrap();
        assert_eq!(a.data["found"], false);
    }

    #[test]
    fn missing_binary_is_clear_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".doctest.toml"), "[project]\nname=\"x\"\n").unwrap();
        let cfg = Config::find(d.path()).unwrap();
        let mut api = LiveCodemap::with_binary(&cfg, PathBuf::from("/nope/codemap"));
        let err = api.call("backend", "project_overview", json!({})).unwrap_err();
        assert!(err.contains("codemap not found at /nope/codemap"), "{err}");
    }

    #[test]
    fn dead_codemap_is_restarted_and_explained() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("codemap");
        let count = d.path().join("count");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> {}\necho 'index is locked' >&2\nexit 3\n",
                count.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(d.path().join(".doctest.toml"), "[project]\nname=\"x\"\n").unwrap();
        let cfg = Config::find(d.path()).unwrap();
        let mut api = LiveCodemap::with_binary(&cfg, script);
        let e1 = api.call("backend", "project_overview", json!({})).unwrap_err();
        assert!(e1.contains("index is locked"), "{e1}");
        let _ = api.call("backend", "project_overview", json!({})).unwrap_err();
        assert_eq!(
            std::fs::read_to_string(&count).unwrap().lines().count(),
            2,
            "a dead codemap must be started again"
        );
    }

    #[test]
    fn a_write_to_a_dead_codemap_counts_as_stopped() {
        let e = write_error(
            "initialize",
            &std::io::Error::from(std::io::ErrorKind::BrokenPipe),
            " (codemap said: index is locked)",
        );
        assert!(
            e.contains("codemap stopped") && e.contains("initialize") && e.contains("index is locked"),
            "{e}"
        );
        assert!(crate::reports::is_internal(&e));
    }

    #[test]
    fn hung_codemap_times_out() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("codemap");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(d.path().join(".doctest.toml"), "[project]\nname=\"x\"\n").unwrap();
        let cfg = Config::find(d.path()).unwrap();
        let mut api = LiveCodemap::with_binary(&cfg, script);
        api.timeout = std::time::Duration::from_millis(500);
        let started = std::time::Instant::now();
        let err = api.call("backend", "project_overview", json!({})).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        assert!(started.elapsed().as_secs() < 5);
    }

    #[test]
    fn stale_flag_read_from_meta() {
        let a = parse_answer(r#"{"data":{},"meta":{"stale":true,"call_id":"c9"}}"#).unwrap();
        assert!(a.stale);
        assert_eq!(a.call_id.as_deref(), Some("c9"));
    }
}
