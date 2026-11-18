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

/// `en`, `id`, `zh`, `pt-BR`, `zh_CN` — not `index`, `messages`.
fn is_language_code(stem: &str) -> bool {
    let (base, region) = match stem.split_once(['-', '_']) {
        Some((b, r)) => (b, Some(r)),
        None => (stem, None),
    };
    (2..=3).contains(&base.len())
        && base.chars().all(|c| c.is_ascii_lowercase())
        && region.is_none_or(|r| (2..=4).contains(&r.len()) && r.chars().all(|c| c.is_ascii_alphanumeric()))
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
    let folder = crate::config::Config::repo_name(&dir);
    let name = composer["name"]
        .as_str()
        .filter(|n| !matches!(*n, "laravel/laravel" | "laravel/lumen" | "laravel/framework"))
        .and_then(|n| n.rsplit('/').next())
        .map(String::from)
        .unwrap_or(folder);
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
    // the menu file: of the files named *menu* that use routeName, the one with the most entries
    let mut menu: Option<(usize, String)> = None;
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
                && is_language_code(stem)
                && !langs.contains(&stem.to_string())
            {
                langs.push(stem.to_string());
            }
            let lower = f
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_lowercase();
            if lower.contains("menu") && matches!(ext, "ts" | "js") {
                let entries = std::fs::read_to_string(f)
                    .map(|t| t.matches("routeName:").count())
                    .unwrap_or(0);
                if entries > 0 && menu.as_ref().is_none_or(|(n, _)| entries > *n) {
                    let rel = f.strip_prefix(&root).unwrap_or(f);
                    menu = Some((entries, format!("{fe}/{}", rel.to_string_lossy())));
                }
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
    t += "[app]   # the running app used for screenshots\nbase_url = \"http://localhost:8000\"   # TODO\nlogin_url = \"/login\"   # TODO\nlogin_steps = [   # only for unattended runs with [app.users]: selectors of your sign-in form\n  { fill = \"input[name=email]\", value = \"$user\" },\n  { fill = \"input[name=password]\", value = \"$password\" },\n  { click = \"button[type=submit]\" },\n]\n\n# Screenshots of signed-in pages: run `doctest login` once and sign in by hand in the window it opens.\n# For unattended runs instead, name env vars holding a test login:\n# [app.users.admin]\n# user_env = \"DOCTEST_USER\"\n# password_env = \"DOCTEST_PASSWORD\"\n\n";
    t += "[screenshots]\nmax_width = 1600\nmask = [   # TODO: replace names and numbers that may be real data\n]\n\n";
    t += "[tests]\nrun = \"php artisan test {filter}\"   # TODO: e.g. docker exec <container> sh -c 'cd /app && php artisan test {filter}'\n\n";
    t += &format!("[guide]\n{}allow_bold = []\n", match menu.as_ref().map(|(_, m)| m) {
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
    fn init_guesses_like_a_person_would() {
        let d = tempfile::tempdir().unwrap();
        let api = d.path().join("shop-api");
        let web = d.path().join("shop-web");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::create_dir_all(web.join("src/i18n/locales")).unwrap();
        std::fs::create_dir_all(web.join("src/config")).unwrap();
        std::fs::create_dir_all(web.join("src/composables")).unwrap();
        std::fs::write(api.join("composer.json"), r#"{"name":"laravel/laravel"}"#).unwrap();
        std::fs::write(web.join("package.json"), r#"{"dependencies":{"vue":"^3.5"}}"#).unwrap();
        std::fs::write(web.join("src/i18n/index.ts"), "export default {}").unwrap();
        std::fs::write(web.join("src/i18n/messages.ts"), "export default {}").unwrap();
        std::fs::write(web.join("src/i18n/locales/en.ts"), "export default {}").unwrap();
        std::fs::write(web.join("src/i18n/locales/zh.ts"), "export default {}").unwrap();
        std::fs::write(
            web.join("src/composables/useAdminMenuTree.ts"),
            "const x = item.routeName",
        )
        .unwrap();
        std::fs::write(
            web.join("src/config/adminMenuGroups.ts"),
            "{ routeName: 'a' }, { routeName: 'b' }, { routeName: 'c' }",
        )
        .unwrap();
        let t = std::fs::read_to_string(run(&api).unwrap()).unwrap();
        assert!(
            t.contains("name = \"shop-api\""),
            "a generic composer name falls back to the folder: {t}"
        );
        assert!(
            t.contains("languages = [\"en\", \"zh\"]"),
            "index/messages are not languages: {t}"
        );
        assert!(
            t.contains("modules_from = \"../shop-web/src/config/adminMenuGroups.ts\""),
            "the file with the most routeName entries: {t}"
        );
    }

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
