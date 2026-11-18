//! Screen text for i18n keys, per language: vue-i18n locale files (JSON / JS / TS modules) and Laravel lang files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default)]
pub struct Labels {
    pub by_lang: BTreeMap<String, BTreeMap<String, String>>,
    hash: String,
}

const LOCALE_DIRS: [&str; 4] = ["locales", "locale", "lang", "i18n"];

impl Labels {
    pub fn load(frontends: &[PathBuf], backend: &Path, languages: &[String]) -> Labels {
        let mut files: Vec<(String, String, PathBuf)> = Vec::new(); // (lang, key prefix, file)
        for fe in frontends {
            let mut found = Vec::new();
            walk(&crate::config::Config::src_root(fe), &mut found);
            for f in found {
                let (Some(stem), Some(ext), Some(parent)) = (
                    f.file_stem().and_then(|s| s.to_str()),
                    f.extension().and_then(|s| s.to_str()),
                    f.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
                ) else {
                    continue;
                };
                if !matches!(ext, "json" | "js" | "ts") {
                    continue;
                }
                let grandparent = f
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if languages.iter().any(|l| l == stem) && LOCALE_DIRS.contains(&parent) {
                    files.push((stem.to_string(), String::new(), f));
                } else if languages.iter().any(|l| l == parent) && LOCALE_DIRS.contains(&grandparent) {
                    // Vue 2 style: lang/en/index.js and lang/en/<part>.js
                    let prefix = if stem == "index" {
                        String::new()
                    } else {
                        stem.to_string()
                    };
                    files.push((parent.to_string(), prefix, f));
                }
            }
        }
        for base in [backend.join("lang"), backend.join("resources/lang")] {
            for lang in languages {
                let json = base.join(format!("{lang}.json"));
                if json.is_file() {
                    files.push((lang.clone(), String::new(), json));
                }
                if let Ok(entries) = std::fs::read_dir(base.join(lang)) {
                    let mut php: Vec<PathBuf> = entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|e| e == "php"))
                        .collect();
                    php.sort();
                    for p in php {
                        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
                        files.push((lang.clone(), stem, p));
                    }
                }
            }
        }
        files.sort();
        let mut by_lang: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut hasher = Sha256::new();
        for (lang, prefix, file) in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            hasher.update(file.to_string_lossy().as_bytes());
            hasher.update(text.as_bytes());
            let map = by_lang.entry(lang).or_default();
            for (k, v) in parse_locale(&text) {
                let key = if prefix.is_empty() { k } else { format!("{prefix}.{k}") };
                map.insert(key, v);
            }
        }
        Labels {
            by_lang,
            hash: crate::hash::hex(&hasher.finalize()),
        }
    }

    /// Text for a dotted key. Falls back to the key without its leading segments, because apps often nest a flat
    /// file under a namespace at runtime (`message.tenant` → `tenant`).
    pub fn text(&self, lang: &str, key: &str) -> Option<&str> {
        let map = self.by_lang.get(lang)?;
        let mut k = key;
        loop {
            if let Some(v) = map.get(k) {
                return Some(v);
            }
            k = k.split_once('.')?.1;
        }
    }

    /// A key's text, also when the key is given without its namespace (`saved` for `messages.saved`).
    pub fn resolve(&self, lang: &str, key: &str) -> Option<&str> {
        if let Some(v) = self.text(lang, key) {
            return Some(v);
        }
        let map = self.by_lang.get(lang)?;
        let suffix = format!(".{key}");
        map.iter().find(|(k, _)| k.ends_with(&suffix)).map(|(_, v)| v.as_str())
    }

    /// Changes whenever any locale file read changes.
    pub fn files_hash(&self) -> &str {
        &self.hash
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if name != "node_modules" && name != "dist" && !name.to_string_lossy().starts_with('.') {
                walk(&p, out);
            }
        } else {
            out.push(p);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Ident(String),
    Punct(char), // { } [ ] : , and '>' for =>
}

fn tokens(text: &str) -> Vec<Tok> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '/' && c.get(i + 1) == Some(&'/') || ch == '#' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if ch == '\'' || ch == '"' || ch == '`' {
            let q = ch;
            let mut s = String::new();
            let mut dynamic = false;
            i += 1;
            while i < c.len() && c[i] != q {
                if c[i] == '\\' && i + 1 < c.len() {
                    i += 1;
                    s.push(match c[i] {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    if q == '`' && c[i] == '$' && c.get(i + 1) == Some(&'{') {
                        dynamic = true;
                    }
                    s.push(c[i]);
                }
                i += 1;
            }
            i += 1;
            out.push(if dynamic {
                Tok::Ident("<dynamic>".into())
            } else {
                Tok::Str(s)
            });
        } else if ch == '=' && c.get(i + 1) == Some(&'>') {
            out.push(Tok::Punct('>'));
            i += 2;
        } else if ch == '=' {
            out.push(Tok::Punct('='));
            i += 1;
        } else if "{}[]:,".contains(ch) {
            out.push(Tok::Punct(ch));
            i += 1;
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' || ch == '.' || ch == '-' {
            let mut s = String::new();
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$' || c[i] == '.' || c[i] == '-') {
                s.push(c[i]);
                i += 1;
            }
            out.push(Tok::Ident(s));
        } else {
            i += 1;
        }
    }
    out
}

/// Flattened `dotted.key → text` from a JSON object, a JS/TS module exporting an object literal, or a PHP file
/// returning an array. Non-string values are skipped.
pub fn parse_locale(text: &str) -> Vec<(String, String)> {
    let toks = tokens(text);
    let Some(start) = root_container(&toks) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut pos = start;
    container(&toks, &mut pos, "", &mut out);
    out
}

/// The literal that holds the messages: after `export default`, after `=` (`module.exports =`, `const x =`), after
/// `return` (PHP), else the first container.
fn root_container(toks: &[Tok]) -> Option<usize> {
    let open = |i: usize| matches!(toks.get(i), Some(Tok::Punct('{')) | Some(Tok::Punct('[')));
    let ident = |i: usize, s: &str| matches!(toks.get(i), Some(Tok::Ident(x)) if x == s);
    (0..toks.len())
        .find(|&i| ident(i, "export") && ident(i + 1, "default") && open(i + 2))
        .map(|i| i + 2)
        .or_else(|| {
            (0..toks.len())
                .find(|&i| toks[i] == Tok::Punct('=') && open(i + 1))
                .map(|i| i + 1)
        })
        .or_else(|| {
            (0..toks.len())
                .find(|&i| ident(i, "return") && open(i + 1))
                .map(|i| i + 1)
        })
        .or_else(|| toks.iter().position(|t| matches!(t, Tok::Punct('{') | Tok::Punct('['))))
}

/// Parses the container opening at `toks[*pos]`, leaves `*pos` after its closing bracket.
fn container(toks: &[Tok], pos: &mut usize, prefix: &str, out: &mut Vec<(String, String)>) {
    let close = if toks[*pos] == Tok::Punct('{') { '}' } else { ']' };
    *pos += 1;
    while *pos < toks.len() {
        match &toks[*pos] {
            Tok::Punct(c) if *c == close => {
                *pos += 1;
                return;
            }
            Tok::Punct(',') => *pos += 1,
            Tok::Str(k) | Tok::Ident(k)
                if matches!(toks.get(*pos + 1), Some(Tok::Punct(':')) | Some(Tok::Punct('>'))) =>
            {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                *pos += 2;
                value(toks, pos, &key, out);
            }
            _ => value(toks, pos, "", out), // array element or something we do not understand: skip it
        }
    }
}

fn value(toks: &[Tok], pos: &mut usize, key: &str, out: &mut Vec<(String, String)>) {
    match toks.get(*pos) {
        Some(Tok::Punct('{')) | Some(Tok::Punct('[')) => {
            if key.is_empty() {
                let mut ignore = Vec::new();
                container(toks, pos, "<skip>", &mut ignore);
            } else {
                container(toks, pos, key, out);
            }
        }
        Some(Tok::Str(s)) => {
            // `'a' + 'b'`-style concatenation is not supported; a lone string is the value.
            if !key.is_empty() && !matches!(toks.get(*pos + 1), Some(Tok::Ident(_))) {
                out.push((key.to_string(), s.clone()));
            }
            *pos += 1;
        }
        Some(_) => *pos += 1,
        None => {}
    }
}

/// A literal (JSON, JS/TS object or array, PHP array) as a tree, for config files such as menus.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Keyed entries and unkeyed elements of one container.
    Map(Vec<(String, Node)>, Vec<Node>),
    Str(String),
    Other,
}

impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries, _) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Node::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// The literal assigned with `=` (`export const x = {…}`), else the first container in the text.
pub fn parse_tree(text: &str) -> Option<Node> {
    let toks = tokens(text);
    let is_open = |t: Option<&Tok>| matches!(t, Some(Tok::Punct('{')) | Some(Tok::Punct('[')));
    let start = (0..toks.len())
        .find(|&i| toks[i] == Tok::Punct('=') && is_open(toks.get(i + 1)))
        .map(|i| i + 1)
        .or_else(|| toks.iter().position(|t| is_open(Some(t))))?;
    let mut pos = start;
    Some(tree_node(&toks, &mut pos))
}

fn tree_node(toks: &[Tok], pos: &mut usize) -> Node {
    match toks.get(*pos) {
        Some(Tok::Punct('{')) | Some(Tok::Punct('[')) => {
            let close = if toks[*pos] == Tok::Punct('{') { '}' } else { ']' };
            *pos += 1;
            let (mut entries, mut elements) = (Vec::new(), Vec::new());
            while *pos < toks.len() {
                match &toks[*pos] {
                    Tok::Punct(c) if *c == close => {
                        *pos += 1;
                        break;
                    }
                    Tok::Punct(',') => *pos += 1,
                    Tok::Punct('}') | Tok::Punct(']') => {
                        // mismatched bracket: stop here so the caller can recover
                        *pos += 1;
                        break;
                    }
                    Tok::Str(k) | Tok::Ident(k)
                        if matches!(toks.get(*pos + 1), Some(Tok::Punct(':')) | Some(Tok::Punct('>'))) =>
                    {
                        let key = k.clone();
                        *pos += 2;
                        entries.push((key, tree_node(toks, pos)));
                    }
                    _ => elements.push(tree_node(toks, pos)),
                }
            }
            Node::Map(entries, elements)
        }
        Some(Tok::Str(s)) => {
            *pos += 1;
            Node::Str(s.clone())
        }
        Some(_) => {
            *pos += 1;
            Node::Other
        }
        None => Node::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn shop() -> Labels {
        Labels::load(
            &[root().join("shop/web")],
            &root().join("shop"),
            &["en".into(), "id".into()],
        )
    }

    #[test]
    fn json_nested_keys() {
        assert_eq!(shop().text("en", "message.save"), Some("Save"));
    }

    #[test]
    fn second_language() {
        assert_eq!(shop().text("id", "message.save"), Some("Simpan"));
    }

    #[test]
    fn vue2_js_module() {
        let l = Labels::load(&[root().join("shop-web2")], &root().join("none"), &["en".into()]);
        assert_eq!(l.text("en", "message.save"), Some("Save"));
        assert_eq!(l.text("en", "message.rentalStartDate"), Some("Rental Start Date"));
        assert_eq!(l.text("en", "message.giro"), Some("Pick the bank's GIRO file"));
        assert_eq!(l.text("en", "message.count"), None);
    }

    #[test]
    fn flat_file_resolves_prefixed_key() {
        // a flat en.ts (`tenant: 'Tenant'`) that messages.ts nests under `message` at runtime
        let mut by_lang = BTreeMap::new();
        by_lang.insert(
            "en".to_string(),
            BTreeMap::from([("tenant".to_string(), "Tenant".to_string())]),
        );
        let l = Labels {
            by_lang,
            hash: String::new(),
        };
        assert_eq!(l.text("en", "message.tenant"), Some("Tenant"));
    }

    #[test]
    fn laravel_php_lang() {
        let l = shop();
        assert_eq!(
            l.text("en", "validation.required"),
            Some("The :attribute field is required.")
        );
        assert_eq!(
            l.text("en", "validation.between.numeric"),
            Some("The :attribute must be between :min and :max.")
        );
    }

    #[test]
    fn module_with_imports_before_the_export() {
        let t = "import { common } from './common'\nimport type { Messages } from './types'\nexport default {\n  message: { save: 'Save' },\n  ...common,\n}\n";
        assert_eq!(parse_locale(t), vec![("message.save".to_string(), "Save".to_string())]);
    }

    #[test]
    fn vue2_folder_per_language() {
        let d = tempfile::tempdir().unwrap();
        let en = d.path().join("web/src/lang/en");
        std::fs::create_dir_all(&en).unwrap();
        std::fs::write(
            en.join("index.js"),
            "import orders from './orders'\nexport default { save: 'Save', orders }\n",
        )
        .unwrap();
        std::fs::write(en.join("orders.js"), "export default { title: 'Orders' }\n").unwrap();
        let l = Labels::load(&[d.path().join("web")], &d.path().join("none"), &["en".into()]);
        assert_eq!(l.text("en", "save"), Some("Save"));
        assert_eq!(l.text("en", "orders.title"), Some("Orders"));
    }

    #[test]
    fn frontend_without_src_folder() {
        let d = tempfile::tempdir().unwrap();
        let loc = d.path().join("resources/js/locales");
        std::fs::create_dir_all(&loc).unwrap();
        std::fs::write(loc.join("en.json"), "{\"save\": \"Save\"}").unwrap();
        let l = Labels::load(&[d.path().join("resources/js")], &d.path().join("none"), &["en".into()]);
        assert_eq!(l.text("en", "save"), Some("Save"));
    }

    #[test]
    fn unknown_key_none() {
        assert_eq!(shop().text("en", "message.nope"), None);
    }

    #[test]
    fn hash_changes_with_content() {
        let a = shop().files_hash().to_string();
        let b = Labels::load(&[root().join("shop-web2")], &root().join("none"), &["en".into()])
            .files_hash()
            .to_string();
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn resolves_keys_without_namespace() {
        let mut l = Labels::default();
        l.by_lang.insert(
            "en".into(),
            BTreeMap::from([(
                "messages.recordSuccessfullyUploaded".to_string(),
                "Record uploaded.".to_string(),
            )]),
        );
        assert_eq!(l.resolve("en", "recordSuccessfullyUploaded"), Some("Record uploaded."));
        assert_eq!(
            l.resolve("en", "messages.recordSuccessfullyUploaded"),
            Some("Record uploaded.")
        );
        assert_eq!(l.resolve("en", "nothing"), None);
    }
}
