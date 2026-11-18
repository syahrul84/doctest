//! `doctest doctor`: checks everything doctest needs — its helpers (codemap, git, curl, gh, Chrome), the project
//! settings, the apps, languages, the test login for screenshots, the test command and the state file — and says what
//! to fix.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

fn check(name: &str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status,
        detail: detail.into(),
    }
}

fn version_of(bin: &Path, arg: &str) -> Option<String> {
    Command::new(bin)
        .arg(arg)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
}

fn on_path(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn run(cfg: Option<&Config>, start: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let newer = crate::update::known_newer()
        .map(|n| format!("; {n} is available, run `doctest update`"))
        .unwrap_or_default();
    out.push(check(
        "doctest",
        if newer.is_empty() { Status::Ok } else { Status::Warn },
        format!("{}{newer}", crate::update::LONG_VERSION),
    ));
    let cm = crate::codemap::binary();
    out.push(match version_of(&cm, "--version") {
        Some(v) => check("codemap", Status::Ok, format!("{v} at {}", cm.display())),
        None => check(
            "codemap",
            Status::Fail,
            format!("not found at {} — install codemap or set CODEMAP_BIN", cm.display()),
        ),
    });
    out.push(if on_path("git") {
        check("git", Status::Ok, "found")
    } else {
        check("git", Status::Fail, "git is needed for change scopes")
    });
    out.push(if on_path("curl") && on_path("tar") {
        check("curl", Status::Ok, "curl and tar found (updates)")
    } else {
        check("curl", Status::Warn, "curl and tar are needed for `doctest update`")
    });
    let gh = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .is_ok_and(|o| o.status.success());
    out.push(if gh {
        check("gh", Status::Ok, "signed in (problem reports can be filed as issues)")
    } else {
        check(
            "gh",
            Status::Warn,
            "gh is missing or not signed in: `doctest reports --send` prints a link instead",
        )
    });
    let chrome = std::env::var("CHROME_PATH")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            [
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "/usr/bin/google-chrome",
                "/usr/bin/chromium",
            ]
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.is_file())
        });
    out.push(match chrome {
        Some(p) => check("chrome", Status::Ok, format!("{} (screenshots)", p.display())),
        None => check(
            "chrome",
            Status::Warn,
            "Chrome not found: screenshots need it (or set CHROME_PATH)",
        ),
    });
    let Some(cfg) = cfg else {
        out.push(check(
            "config",
            Status::Fail,
            format!(
                "no .doctest.toml in {} or above — run `doctest init` in the Laravel repo",
                start.display()
            ),
        ));
        return out;
    };
    out.push(check(
        "config",
        Status::Ok,
        format!("{}", cfg.root.join(crate::config::FILE).display()),
    ));
    out.push(if cfg.backend.join("composer.json").is_file() {
        check("backend", Status::Ok, cfg.backend.display().to_string())
    } else {
        check(
            "backend",
            Status::Fail,
            format!("{} has no composer.json", cfg.backend.display()),
        )
    });
    for fe in &cfg.frontends {
        let name = format!("frontend {}", Config::repo_name(fe));
        out.push(if fe.join("package.json").is_file() {
            check(&name, Status::Ok, fe.display().to_string())
        } else {
            check(&name, Status::Fail, format!("{} has no package.json", fe.display()))
        });
    }
    let labels = crate::i18n::Labels::load(&cfg.frontends, &cfg.backend, &cfg.languages);
    let missing: Vec<&String> = cfg
        .languages
        .iter()
        .filter(|l| labels.by_lang.get(*l).is_none_or(|m| m.is_empty()))
        .collect();
    out.push(if missing.is_empty() {
        check(
            "languages",
            Status::Ok,
            cfg.languages
                .iter()
                .map(|l| format!("{l}: {} labels", labels.by_lang.get(l).map(|m| m.len()).unwrap_or(0)))
                .collect::<Vec<_>>()
                .join(", "),
        )
    } else {
        check(
            "languages",
            Status::Warn,
            format!(
                "no locale file found for {} — UI names cannot be checked in that language",
                missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
        )
    });
    if let Some(m) = &cfg.guide.modules_from {
        out.push(if m.is_file() {
            check("menu", Status::Ok, m.display().to_string())
        } else {
            check(
                "menu",
                Status::Warn,
                format!("[guide] modules_from {} does not exist", m.display()),
            )
        });
    }
    // sign-in for screenshots: one per portal (or one for the app)
    let targets = cfg.login_targets();
    let unattended = |name: &str| {
        cfg.creds_user(name)
            .and_then(|u| cfg.app.users.iter().find(|(n, _)| *n == u).map(|(_, c)| c.clone()))
            .is_some_and(|c| std::env::var(&c.user_env).is_ok() && std::env::var(&c.password_env).is_ok())
    };
    let missing: Vec<String> = targets
        .iter()
        .filter(|(n, _)| !crate::shots::signed_in(cfg, n) && !unattended(n))
        .map(|(n, _)| n.clone())
        .collect();
    out.push(if targets.is_empty() {
        check(
            "test login",
            Status::Ok,
            "no sign-in page in the settings: portals are detected from the app's login pages when doctest runs, and `doctest login` signs in to each",
        )
    } else if missing.is_empty() {
        check("test login", Status::Ok, format!("signed in: {}", targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")))
    } else {
        check(
            "test login",
            Status::Warn,
            format!(
                "not signed in to {}: run `doctest login` (a Chrome window opens per portal; sign in by hand)",
                missing.join(", ")
            ),
        )
    });
    let first = cfg.tests.run.split_whitespace().next().unwrap_or_default().to_string();
    out.push(if on_path(&first) {
        check("tests", Status::Ok, format!("`{}`", cfg.tests.run))
    } else {
        check(
            "tests",
            Status::Warn,
            format!("`{first}` (from [tests] run) is not on PATH"),
        )
    });
    out.push(match crate::state::State::load_strict(cfg) {
        Ok(_) => check(
            "state",
            Status::Ok,
            crate::state::State::path(cfg).display().to_string(),
        ),
        Err(e) => check("state", Status::Fail, e),
    });
    let reports = crate::reports::load().iter().filter(|r| r.sent.is_none()).count();
    if reports > 0 {
        out.push(check(
            "problem reports",
            Status::Warn,
            format!("{reports} not sent — `doctest reports` lists them"),
        ));
    }
    out
}

pub fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        let mark = match c.status {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        };
        s += &format!("{mark}  {:<16} {}\n", c.name, c.detail);
    }
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    s + &if fails == 0 {
        "doctor: nothing blocks doctest".to_string()
    } else {
        format!("doctor: {fails} problem(s) to fix first")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_each_part() {
        let (_d, mut cfg) = crate::work::tests::project();
        cfg.app.login_url = "/login".into();
        let checks = run(Some(&cfg), &cfg.root);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        for want in [
            "doctest",
            "codemap",
            "git",
            "curl",
            "gh",
            "chrome",
            "config",
            "backend",
            "frontend web",
            "languages",
            "test login",
            "state",
        ] {
            assert!(names.contains(&want), "{want} not in {names:?}");
        }
        let login = checks.iter().find(|c| c.name == "test login").unwrap();
        assert_eq!(login.status, Status::Warn, "{login:?}");
        assert!(login.detail.contains("doctest login"), "{login:?}");
        assert!(render(&checks).contains("doctest"));
    }

    #[test]
    fn doctor_without_config() {
        let d = tempfile::tempdir().unwrap();
        let checks = run(None, d.path());
        let cfg = checks.iter().find(|c| c.name == "config").unwrap();
        assert_eq!(cfg.status, Status::Fail);
        assert!(cfg.detail.contains("doctest init"));
    }
}
