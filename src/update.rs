//! `doctest update`: replaces this program with the latest GitHub release of doctest, after checking it against the
//! release's SHA256SUMS. Also the once-a-day check behind the MCP server's "new version" notice. Uses the system's
//! `curl` and `tar`. Until the first release exists, `update` says so and changes nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::reports::REPO;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The version with the commit and date it was built from (build.rs).
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("DOCTEST_BUILD"), ")");

/// The release file for this machine.
fn asset() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("doctest-aarch64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("doctest-x86_64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("doctest-x86_64-unknown-linux-gnu.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("doctest-aarch64-unknown-linux-gnu.tar.gz")
    } else {
        None
    }
}

fn curl(url: &str, timeout_s: u32) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", &timeout_s.to_string()])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", &format!("User-Agent: doctest/{VERSION}")])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "`curl` is needed to download updates and was not found".to_string())?;
    if !out.status.success() {
        return Err(format!(
            "downloading {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// The latest release's tag, e.g. `v0.1.0`.
pub fn latest(timeout_s: u32) -> Result<String, String> {
    let body = curl(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        timeout_s,
    )
    .map_err(|e| {
        if e.contains("404") {
            format!("doctest has no release on github.com/{REPO} yet")
        } else {
            e
        }
    })?;
    let v: Value = serde_json::from_slice(&body).map_err(|_| "unexpected reply from GitHub".to_string())?;
    v["tag_name"]
        .as_str()
        .map(str::to_owned)
        .ok_or("GitHub's reply has no release tag".into())
}

fn parts(version: &str) -> Vec<u64> {
    version
        .trim_start_matches('v')
        .split(['.', '-'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

pub fn is_newer(tag: &str, current: &str) -> bool {
    parts(tag) > parts(current)
}

fn cache() -> PathBuf {
    crate::reports::tool_root().join("update-check.json")
}

/// The newer version a check found (at most a day old), if any.
pub fn known_newer() -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cache()).ok()?).ok()?;
    let tag = v["latest"].as_str()?;
    is_newer(tag, VERSION).then(|| tag.trim_start_matches('v').to_string())
}

/// Ask GitHub when the last check is a day old, without delaying the caller (DOCTEST_NO_UPDATE_CHECK turns it off).
pub fn check_in_background() {
    if std::env::var_os("DOCTEST_NO_UPDATE_CHECK").is_some() {
        return;
    }
    let due = std::fs::read_to_string(cache())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["checked"].as_i64())
        .is_none_or(|t| chrono::Utc::now().timestamp() - t > 86_400);
    if !due {
        return;
    }
    std::thread::spawn(|| {
        let latest = latest(10).ok();
        let _ = std::fs::create_dir_all(crate::reports::tool_root());
        let _ = std::fs::write(
            cache(),
            json!({"checked": chrono::Utc::now().timestamp(), "latest": latest}).to_string(),
        );
    });
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::hash::hex(&Sha256::digest(bytes))
}

fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines()
        .find_map(|l| l.split_once("  ").filter(|(_, f)| f.trim() == asset))
        .map(|(h, _)| h.trim().to_owned())
}

/// Puts `new` where `exe` is (a rename over the running program; it keeps running until it exits).
fn replace_exe(new: &Path, exe: &Path) -> std::io::Result<()> {
    let staged = exe.with_extension("new");
    let result = (|| {
        std::fs::copy(new, &staged)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&staged, exe)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// The first `doctest` on PATH, when it is not this program.
fn shadowed_by(exe: &Path, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let first = std::env::split_paths(path)
        .map(|dir| dir.join("doctest"))
        .find(|p| p.is_file())?;
    let first = std::fs::canonicalize(&first).unwrap_or(first);
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    (first != exe).then_some(first)
}

fn path_warning(exe: &Path) -> String {
    match std::env::var_os("PATH").and_then(|p| shadowed_by(exe, &p)) {
        Some(other) => format!(
            "\nWarning: `doctest` on PATH starts {}, not {}; remove it or put {} earlier on PATH.",
            other.display(),
            exe.display(),
            exe.parent().map(|p| p.display().to_string()).unwrap_or_default()
        ),
        None => String::new(),
    }
}

pub fn run(check_only: bool, force: bool) -> Result<String, String> {
    let here = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| e.to_string())?;
    let tag = latest(20)?;
    let _ = std::fs::create_dir_all(crate::reports::tool_root());
    let _ = std::fs::write(
        cache(),
        json!({"checked": chrono::Utc::now().timestamp(), "latest": tag}).to_string(),
    );
    let new = tag.trim_start_matches('v').to_string();
    if !force && !is_newer(&tag, VERSION) {
        return Ok(format!(
            "doctest {LONG_VERSION} is the latest version.{}",
            path_warning(&here)
        ));
    }
    let notes = format!("Release notes: https://github.com/{REPO}/releases/tag/{tag}");
    if check_only {
        return Ok(format!(
            "doctest {new} is available (this is {LONG_VERSION}). {notes}\nRun `doctest update` to install it.{}",
            path_warning(&here)
        ));
    }
    let Some(asset) = asset() else {
        return Err(format!(
            "there is no ready-made doctest for this system; build it with `cargo install --locked --git https://github.com/{REPO} --tag {tag}`"
        ));
    };
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let archive = curl(&format!("{base}/{asset}"), 300)?;
    let sums = String::from_utf8(curl(&format!("{base}/SHA256SUMS"), 30)?).map_err(|e| e.to_string())?;
    let expected = checksum_for(&sums, asset).ok_or(format!("SHA256SUMS has no line for {asset}"))?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        return Err(format!(
            "the download does not match its checksum ({actual} != {expected}); nothing was changed"
        ));
    }
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let tarball = work.path().join(asset);
    std::fs::write(&tarball, &archive).map_err(|e| e.to_string())?;
    let ok = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(work.path())
        .status()
        .map_err(|_| "`tar` is needed to unpack the update".to_string())?
        .success();
    let unpacked = work.path().join("doctest");
    if !ok || !unpacked.is_file() {
        return Err(format!("could not unpack {asset}"));
    }
    let said = Command::new(&unpacked)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    let said = String::from_utf8_lossy(&said.stdout).to_string();
    if !said.contains(&new) {
        return Err(format!(
            "the downloaded program reports `{}`, expected {new}; nothing was changed",
            said.trim()
        ));
    }
    replace_exe(&unpacked, &here).map_err(|e| {
        format!(
            "cannot replace {}: {e} (run it with sudo, or install into a folder you own)",
            here.display()
        )
    })?;
    Ok(format!(
        "Updated {} to {new}. {notes}\nAI sessions use it from their next start (or restart the MCP server).{}",
        here.display(),
        path_warning(&here)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.3.0", "0.2.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(LONG_VERSION.starts_with(VERSION) && LONG_VERSION.contains('('));
    }

    #[test]
    fn replaces_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("doctest");
        let new = dir.path().join("download.bin");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace_exe(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!exe.with_extension("new").exists());
    }

    #[test]
    fn finds_another_copy_first_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (cargo, mine, empty) = (root.join("cargo-bin"), root.join("bin"), root.join("other"));
        for d in [&cargo, &mine, &empty] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(cargo.join("doctest"), "old").unwrap();
        std::fs::write(mine.join("doctest"), "new").unwrap();
        let exe = mine.join("doctest");
        let path = |dirs: &[&PathBuf]| std::env::join_paths(dirs.iter()).unwrap();
        assert_eq!(
            shadowed_by(&exe, &path(&[&empty, &cargo, &mine])),
            Some(cargo.join("doctest"))
        );
        assert_eq!(shadowed_by(&exe, &path(&[&empty, &mine, &cargo])), None);
    }

    #[test]
    fn checksum_line_for_the_asset() {
        let sums = "abc  doctest-aarch64-apple-darwin.tar.gz\ndef  doctest-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "doctest-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("def")
        );
        assert_eq!(checksum_for(sums, "nope.tar.gz"), None);
    }
}
