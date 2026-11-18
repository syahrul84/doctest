//! `doctest init`: writes .doctest.toml for the backend repo, filling what it can find and marking the rest TODO.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn json(p: &Path) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn uses_vue(dir: &Path) -> bool {
    let p = json(&dir.join("package.json"));
    ["dependencies", "devDependencies"]
        .iter()
        .any(|k| p[k].get("vue").is_some())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let n = e.file_name();
        if p.is_dir() {
            if n != "node_modules" && n != "dist" && !n.to_string_lossy().starts_with('.') {
                walk(&p, out, depth + 1);
            }
        } else {
            out.push(p);
        }
    }
}

pub fn run(dir: &Path) -> Result<PathBuf, String> {
    let dir = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(crate::config::FILE);
    if path.exists() {
        return Err(format!("{} already exists; edit it instead", path.display()));
    }
    let composer = json(&dir.join("composer.json"));
    if composer.is_null() {
        return Err(format!(
            "{} has no composer.json; run doctest init in the Laravel (backend) repo",
            dir.display()
        ));
    }
    let name = composer["name"]
        .as_str()
        .and_then(|n| n.rsplit('/').next())
        .map(String::from)
        .unwrap_or_else(|| crate::config::Config::repo_name(&dir));
    let mut frontends: Vec<String> = Vec::new();
    if let Some(parent) = dir.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        let mut sibs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && *p != dir && uses_vue(p))
            .collect();
        sibs.sort();
        for s in sibs {
            frontends.push(format!("../{}", s.file_name().unwrap_or_default().to_string_lossy()));
        }
    }
    let mut langs: Vec<String> = Vec::new();
    let mut menu: Option<String> = None;
    for fe in &frontends {
        let root = dir.join(fe);
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files, 0);
        files.sort();
        for f in &files {
            let parent = f
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let ext = f.extension().and_then(|s| s.to_str()).unwrap_or_default();
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if matches!(parent, "locales" | "locale" | "lang" | "i18n")
                && matches!(ext, "json" | "ts" | "js")
                && stem.len() <= 5
                && !langs.contains(&stem.to_string())
            {
                langs.push(stem.to_string());
            }
            let lower = f
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_lowercase();
            if menu.is_none()
                && lower.contains("menu")
                && matches!(ext, "ts" | "js")
                && std::fs::read_to_string(f).is_ok_and(|t| t.contains("routeName"))
            {
                let rel = f.strip_prefix(&root).unwrap_or(f);
                menu = Some(format!("{fe}/{}", rel.to_string_lossy()));
            }
        }
    }
    langs.sort();
    if let Some(i) = langs.iter().position(|l| l == "en") {
        let en = langs.remove(i);
        langs.insert(0, en);
    }
    if langs.is_empty() {
        langs.push("en".into());
    }
    let q = |v: &[String]| v.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
    let mut t = format!(
        "# doctest settings — see https://github.com/syahrul84/doctest\n[project]\nname = \"{name}\"\nfrontends = [{}]{}\nlanguages = [{}]   # first is the primary language\nout = \"document\"\n\n",
        q(&frontends),
        if frontends.is_empty() {
            "   # TODO: paths of the Vue apps, relative to this file"
        } else {
            ""
        },
        q(&langs)
    );
    t += "[app]   # the running app used for screenshots\nbase_url = \"http://localhost:8000\"   # TODO\nlogin_url = \"/login\"   # TODO\nlogin_steps = [   # TODO: selectors of your sign-in form\n  { fill = \"input[name=email]\", value = \"$user\" },\n  { fill = \"input[name=password]\", value = \"$password\" },\n  { click = \"button[type=submit]\" },\n]\n\n[app.users.admin]   # TODO: set these env vars to a test admin account\nuser_env = \"DOCTEST_USER\"\npassword_env = \"DOCTEST_PASSWORD\"\n\n";
    t += "[screenshots]\nmax_width = 1600\nmask = [   # TODO: replace names and numbers that may be real data\n]\n\n";
    t += "[tests]\nrun = \"php artisan test {filter}\"   # TODO: e.g. docker exec <container> sh -c 'cd /app && php artisan test {filter}'\n\n";
    t += &format!("[guide]\n{}allow_bold = []\n", match &menu {
        Some(m) => format!("modules_from = \"{m}\"\n"),
        None => "# modules_from = \"../web/src/config/menu.ts\"   # TODO: menu file with routeName entries, for chapters\n".into(),
    });
    std::fs::write(&path, t).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_toml_with_todos() {
        let d = tempfile::tempdir().unwrap();
        let api = d.path().join("api");
        let web = d.path().join("web");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::create_dir_all(web.join("src/i18n/locales")).unwrap();
        std::fs::create_dir_all(web.join("src/config")).unwrap();
        std::fs::write(
            api.join("composer.json"),
            r#"{"name":"acme/shop-api","require":{"laravel/framework":"^13.0"}}"#,
        )
        .unwrap();
        std::fs::write(web.join("package.json"), r#"{"dependencies":{"vue":"^3.5"}}"#).unwrap();
        std::fs::write(web.join("src/i18n/locales/id.ts"), "export default {}").unwrap();
        std::fs::write(web.join("src/i18n/locales/en.ts"), "export default {}").unwrap();
        std::fs::write(
            web.join("src/config/adminMenu.ts"),
            "export const m = { a: [{ label: 'X', routeName: 'x' }] }",
        )
        .unwrap();
        let path = run(&api).unwrap();
        let t = std::fs::read_to_string(&path).unwrap();
        assert!(t.contains("name = \"shop-api\""), "{t}");
        assert!(t.contains("frontends = [\"../web\"]"), "{t}");
        assert!(t.contains("languages = [\"en\", \"id\"]"), "{t}");
        assert!(t.contains("modules_from = \"../web/src/config/adminMenu.ts\""), "{t}");
        assert!(t.contains("# TODO"), "{t}");
        crate::config::Config::load(&path).unwrap();
        assert!(run(&api).unwrap_err().contains("already exists"));
    }
}
