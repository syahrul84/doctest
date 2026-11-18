//! Git facts: the current commit and what changed since a commit.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn head(repo: &Path) -> Result<String, String> {
    Ok(git(repo, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Files changed in `commit..HEAD`, plus uncommitted and untracked ones, relative to the repo.
pub fn changed_since(repo: &Path, commit: &str) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    let committed = git(repo, &["diff", "--name-only", "--relative", &format!("{commit}..HEAD")])?;
    let working = git(repo, &["diff", "--name-only", "--relative", "HEAD"])?;
    let untracked = git(repo, &["ls-files", "--others", "--exclude-standard"])?;
    for line in committed.lines().chain(working.lines()).chain(untracked.lines()) {
        let p = PathBuf::from(line.trim());
        if !line.trim().is_empty() && !files.contains(&p) {
            files.push(p);
        }
    }
    Ok(files)
}

/// Where the current branch left its base: `base`, else origin/HEAD, main, master or develop.
pub fn merge_base(repo: &Path, base: Option<&str>) -> Result<String, String> {
    let candidates: Vec<&str> = match base {
        Some(b) => vec![b],
        None => vec!["origin/HEAD", "main", "master", "develop"],
    };
    for c in &candidates {
        if git(repo, &["rev-parse", "--verify", "--quiet", &format!("{c}^{{commit}}")]).is_ok() {
            return Ok(git(repo, &["merge-base", "HEAD", c])?.trim().to_string());
        }
    }
    Err(format!(
        "no base branch {} in {}; pass base",
        candidates.join(" / "),
        repo.display()
    ))
}

/// Lines added or changed since `commit` (committed or not), per file relative to `repo`; untracked files count whole.
pub fn changed_lines(repo: &Path, commit: &str) -> Result<std::collections::BTreeMap<PathBuf, Vec<usize>>, String> {
    let mut out: std::collections::BTreeMap<PathBuf, Vec<usize>> = std::collections::BTreeMap::new();
    let diff = git(repo, &["diff", "-U0", "--relative", "--no-color", commit])?;
    let mut file: Option<PathBuf> = None;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            file = p.strip_prefix("b/").map(PathBuf::from);
        } else if let (Some(f), Some(h)) = (&file, line.strip_prefix("@@ ")) {
            let Some(plus) = h.split_whitespace().find(|s| s.starts_with('+')) else {
                continue;
            };
            let mut parts = plus[1..].split(',');
            let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            out.entry(f.clone()).or_default().extend(start..start + count);
        }
    }
    for f in git(repo, &["ls-files", "--others", "--exclude-standard"])?
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let n = std::fs::read_to_string(repo.join(f))
            .map(|t| t.lines().count())
            .unwrap_or(0);
        out.insert(PathBuf::from(f), (1..=n.max(1)).collect());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(ok.status.success(), "{:?}", String::from_utf8_lossy(&ok.stderr));
    }

    #[test]
    fn changed_since_includes_untracked() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        sh(p, &["init", "-q"]);
        sh(p, &["config", "user.email", "t@t"]);
        sh(p, &["config", "user.name", "t"]);
        std::fs::write(p.join("a.txt"), "1").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "a"]);
        let first = head(p).unwrap();
        std::fs::write(p.join("c.txt"), "1").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "c"]);
        std::fs::write(p.join("a.txt"), "2").unwrap();
        std::fs::write(p.join("b.txt"), "new").unwrap();
        let mut changed = changed_since(p, &first).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt"), PathBuf::from("c.txt")]
        );
    }

    #[test]
    fn changed_since_is_relative_to_a_subfolder() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        sh(p, &["init", "-q"]);
        sh(p, &["config", "user.email", "t@t"]);
        sh(p, &["config", "user.name", "t"]);
        std::fs::create_dir_all(p.join("web/src")).unwrap();
        std::fs::write(p.join("web/src/a.vue"), "1").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "a"]);
        let first = head(p).unwrap();
        std::fs::write(p.join("web/src/a.vue"), "2").unwrap();
        std::fs::write(p.join("web/src/b.vue"), "2").unwrap();
        let mut changed = changed_since(&p.join("web"), &first).unwrap();
        changed.sort();
        assert_eq!(changed, vec![PathBuf::from("src/a.vue"), PathBuf::from("src/b.vue")]);
    }

    #[test]
    fn changed_lines_from_diff() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        sh(p, &["init", "-q"]);
        sh(p, &["config", "user.email", "t@t"]);
        sh(p, &["config", "user.name", "t"]);
        std::fs::write(p.join("a.php"), "1\n2\n3\n4\n").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "a"]);
        let first = head(p).unwrap();
        std::fs::write(p.join("a.php"), "1\nTWO\n3\n4\nfive\n").unwrap();
        std::fs::write(p.join("n.php"), "x\ny\n").unwrap();
        let lines = changed_lines(p, &first).unwrap();
        assert!(lines[&PathBuf::from("a.php")].contains(&2) && lines[&PathBuf::from("a.php")].contains(&5));
        assert!(!lines[&PathBuf::from("a.php")].contains(&3));
        assert!(lines[&PathBuf::from("n.php")].contains(&1) && lines[&PathBuf::from("n.php")].contains(&2));
    }

    #[test]
    fn merge_base_with_default_branch() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        sh(p, &["init", "-q", "-b", "master"]);
        sh(p, &["config", "user.email", "t@t"]);
        sh(p, &["config", "user.name", "t"]);
        std::fs::write(p.join("a"), "1").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "a"]);
        let base = head(p).unwrap();
        sh(p, &["switch", "-qc", "feat/x"]);
        std::fs::write(p.join("b"), "1").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "b"]);
        assert_eq!(merge_base(p, None).unwrap(), base);
        assert_eq!(merge_base(p, Some("master")).unwrap(), base);
        assert!(merge_base(p, Some("nope")).unwrap_err().contains("nope"));
    }

    #[test]
    fn head_outside_repo_is_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(head(d.path()).is_err());
    }
}
