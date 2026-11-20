//! Progress of long steps, so the AI and the person can see work moving: stderr lines in the CLI, MCP
//! `notifications/progress` in the server.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};

pub trait Progress: Sync {
    fn step(&self, done: usize, total: usize, what: &str);
}

pub fn percent(done: usize, total: usize) -> u32 {
    if total == 0 {
        100
    } else {
        ((done as f64 / total as f64) * 100.0).floor() as u32
    }
}

pub fn line(done: usize, total: usize, what: &str) -> String {
    format!("[{:>3}%] {what} {done}/{total}", percent(done, total))
}

pub struct Silent;

impl Progress for Silent {
    fn step(&self, _: usize, _: usize, _: &str) {}
}

/// Prints a line when the percentage changes or two seconds passed, and always at the end of a step.
pub struct Stderr {
    last: Mutex<(u32, String, Option<Instant>)>,
}

impl Default for Stderr {
    fn default() -> Self {
        Stderr {
            last: Mutex::new((u32::MAX, String::new(), None)),
        }
    }
}

impl Progress for Stderr {
    fn step(&self, done: usize, total: usize, what: &str) {
        let p = percent(done, total);
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let stale = last.2.is_none_or(|t| t.elapsed().as_secs() >= 2);
        if p != last.0 || what != last.1 || stale || done == total {
            eprintln!("{}", line(done, total, what));
            *last = (p, what.to_string(), Some(Instant::now()));
        }
    }
}

pub type Shared = Arc<Mutex<Box<dyn Write + Send>>>;

/// Sends MCP progress notifications for one tool call.
pub struct Mcp {
    token: Value,
    out: Shared,
    last: Mutex<(u32, String)>,
}

impl Mcp {
    pub fn new(token: Value, out: Shared) -> Mcp {
        Mcp {
            token,
            out,
            last: Mutex::new((u32::MAX, String::new())),
        }
    }
}

impl Progress for Mcp {
    fn step(&self, done: usize, total: usize, what: &str) {
        let p = percent(done, total);
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            if p == last.0 && what == last.1 && done != total {
                return;
            }
            *last = (p, what.to_string());
        }
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {
            "progressToken": self.token, "progress": done, "total": total, "message": line(done, total, what)
        }});
        let mut w = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(w, "{msg}");
        let _ = w.flush();
    }
}

/// Keeps every step (tests).
#[cfg(test)]
#[derive(Default)]
pub struct Recorder(Mutex<Vec<(usize, usize, String)>>);

#[cfg(test)]
impl Recorder {
    pub fn steps(&self) -> Vec<(usize, usize, String)> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Progress for Recorder {
    fn step(&self, done: usize, total: usize, what: &str) {
        self.0.lock().unwrap().push((done, total, what.to_string()));
    }
}

/// A Write into a shared Vec (tests).
#[cfg(test)]
pub struct SharedVec(pub Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedVec {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_and_line() {
        assert_eq!(percent(170, 460), 36);
        assert_eq!(percent(0, 0), 100);
        assert_eq!(line(170, 460, "facts"), "[ 36%] facts 170/460");
        assert_eq!(line(3, 3, "tests"), "[100%] tests 3/3");
    }

    #[test]
    fn recorder_keeps_steps() {
        let r = Recorder::default();
        r.step(1, 2, "a");
        r.step(2, 2, "b");
        assert_eq!(r.steps(), vec![(1, 2, "a".to_string()), (2, 2, "b".to_string())]);
    }

    #[test]
    fn mcp_progress_writes_notifications() {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let w: Shared = std::sync::Arc::new(std::sync::Mutex::new(Box::new(SharedVec(sink.clone()))));
        let p = Mcp::new(serde_json::json!("tok-1"), w);
        p.step(5, 10, "facts");
        let text = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["method"], "notifications/progress");
        assert_eq!(v["params"]["progressToken"], "tok-1");
        assert_eq!(v["params"]["progress"], 5);
        assert_eq!(v["params"]["total"], 10);
        assert_eq!(v["params"]["message"], "[ 50%] facts 5/10");
    }
}
