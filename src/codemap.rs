//! Code facts from codemap: one `codemap serve` child per app, spoken to over MCP stdio.
//! Tests use `FixtureCodemap`, which answers from JSON files.

use std::collections::BTreeMap;
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
        }
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
        parse_answer(&text).map_err(|e| format!("codemap {tool}: {e}"))
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
