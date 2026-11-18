//! What Claude writes: Markdown files with a front-matter header, under `document/_source/`.
//! Header style: one `key: value` per line, `[a, b]` lists, ints, true/false (no YAML library needed).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Page,
    Process,
    System,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Page => "page",
            Kind::Process => "process",
            Kind::System => "system",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrontMatter {
    pub title: String,
    pub kind: Kind,
    pub module: Option<String>,
    pub routes: Vec<String>,
    pub covers: Vec<String>,
    pub order: i64,
    pub summary: String,
    pub lang: String,
    /// Hash of the primary-language body this translation was made from.
    pub translated_from: Option<String>,
    pub verified: Option<bool>,
    /// Keys doctest does not use, kept as written.
    pub extra: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// Relative to `_source/`.
    pub path: PathBuf,
    pub fm: FrontMatter,
    pub body: String,
}

fn list(v: &str) -> Vec<String> {
    let v = v.trim();
    let inner = v.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(v);
    inner
        .split(',')
        .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn scalar(v: &str) -> String {
    v.trim().trim_matches(['"', '\'']).to_string()
}

/// `guide/<lang>/…` → lang.
fn lang_from_path(rel: &Path) -> Option<String> {
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned());
    (parts.next()? == "guide").then(|| parts.next()).flatten()
}

impl Source {
    pub fn parse(rel: &Path, text: &str) -> Result<Source, String> {
        let at = rel.display();
        let text = text.replace("\r\n", "\n");
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| format!("{at}: must start with a --- front-matter header"))?;
        let end = rest
            .find("\n---")
            .ok_or_else(|| format!("{at}: front matter has no closing ---"))?;
        let header = &rest[..end];
        let body = rest[end + 4..].trim_start_matches(['\n', '-']).to_string();
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for line in header
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        {
            let (k, v) = line
                .split_once(':')
                .ok_or_else(|| format!("{at}: front matter line is not `key: value`: {line}"))?;
            let v = match v.find(" #") {
                Some(i) if !v.trim_start().starts_with(['"', '\'']) => &v[..i],
                _ => v,
            };
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
        let mut take = |k: &str| map.remove(k);
        let title = take("title")
            .map(|v| scalar(&v))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("{at}: front matter needs a title"))?;
        let kind = match take("kind").map(|v| scalar(&v)).as_deref() {
            Some("page") => Kind::Page,
            Some("process") => Kind::Process,
            Some("system") | None => Kind::System,
            Some(other) => return Err(format!("{at}: kind must be page, process or system, not {other}")),
        };
        let routes = take("routes").map(|v| list(&v)).unwrap_or_default();
        if kind == Kind::Page && routes.len() != 1 {
            return Err(format!(
                "{at}: a page guide needs exactly one route (routes: [<page route name>])"
            ));
        }
        let order = match take("order") {
            Some(v) => scalar(&v)
                .parse()
                .map_err(|_| format!("{at}: order must be a whole number: {v}"))?,
            None => 0,
        };
        let verified = match take("verified").map(|v| scalar(&v)) {
            Some(v) if v == "true" => Some(true),
            Some(v) if v == "false" => Some(false),
            Some(v) => return Err(format!("{at}: verified must be true or false, not {v}")),
            None => None,
        };
        let fm = FrontMatter {
            title,
            kind,
            module: take("module").map(|v| scalar(&v)).filter(|m| !m.is_empty()),
            routes,
            covers: take("covers").map(|v| list(&v)).unwrap_or_default(),
            order,
            summary: take("summary").map(|v| scalar(&v)).unwrap_or_default(),
            lang: take("lang")
                .map(|v| scalar(&v))
                .or_else(|| lang_from_path(rel))
                .unwrap_or_default(),
            translated_from: take("translated_from").map(|v| scalar(&v)).filter(|v| !v.is_empty()),
            verified,
            extra: map,
        };
        Ok(Source {
            path: rel.to_path_buf(),
            fm,
            body,
        })
    }

    /// Header plus body, keys in a fixed order.
    pub fn render_text(&self) -> String {
        let q = |v: &str| -> String {
            if v.contains(" #") || v.starts_with(['"', '\'', '[', '#']) || v.contains(": ") {
                format!("\"{}\"", v.replace('"', "'"))
            } else {
                v.to_string()
            }
        };
        let f = &self.fm;
        let mut h = format!("---\ntitle: {}\nkind: {}\n", q(&f.title), f.kind.as_str());
        if let Some(m) = &f.module {
            h += &format!("module: {m}\n");
        }
        if !f.routes.is_empty() {
            h += &format!("routes: [{}]\n", f.routes.join(", "));
        }
        if !f.covers.is_empty() {
            h += &format!("covers: [{}]\n", f.covers.join(", "));
        }
        h += &format!("order: {}\n", f.order);
        if !f.summary.is_empty() {
            h += &format!("summary: {}\n", q(&f.summary));
        }
        if !f.lang.is_empty() {
            h += &format!("lang: {}\n", f.lang);
        }
        if let Some(t) = &f.translated_from {
            h += &format!("translated_from: {t}\n");
        }
        if let Some(v) = f.verified {
            h += &format!("verified: {v}\n");
        }
        for (k, v) in &f.extra {
            h += &format!("{k}: {v}\n");
        }
        format!("{h}---\n{}", self.body)
    }
}

/// All sources under `document/_source/`.
pub fn sources(cfg: &Config) -> Result<Vec<Source>, String> {
    let root = source_root(cfg);
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let rel = f.strip_prefix(&root).unwrap_or(&f).to_path_buf();
        let text = std::fs::read_to_string(&f).map_err(|e| format!("cannot read {}: {e}", f.display()))?;
        out.push(Source::parse(&rel, &text)?);
    }
    Ok(out)
}

pub fn source_root(cfg: &Config) -> PathBuf {
    cfg.out.join("_source")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// Page key → file-name slug.
pub fn slug(key: &str) -> String {
    // camelCase → camel-case (financialGL → financial-gl)
    let chars: Vec<char> = key.chars().collect();
    let mut split = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() && chars[i - 1].is_ascii_lowercase() {
            split.push('-');
        }
        split.push(c);
    }
    let s: String = split
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

pub fn guide_path(lang: &str, portal: &str, module: &str, page_key: &str) -> PathBuf {
    let mut p = PathBuf::from("guide").join(lang);
    if !portal.is_empty() {
        p.push(slug(portal));
    }
    p.join(slug(module)).join(format!("{}.md", slug(page_key)))
}

/// Hash of a body, ignoring trailing whitespace.
pub fn body_hash(body: &str) -> String {
    crate::hash::sha256(body.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ntitle: Utilities & Other Deposit Schedule\nmodule: gl\nkind: page\nroutes: [admin.financial-gl.report.deposit-schedule]\naros: [financial_admin/deposit_schedule]\norder: 345\nsummary: List the utility and other deposits held, as of a date.\n---\n# Utilities & Other Deposit Schedule\n\nBody.\n";

    #[test]
    fn parses_flat_front_matter() {
        let s = Source::parse(Path::new("guide/en/gl/deposit.md"), SAMPLE).unwrap();
        assert_eq!(s.fm.title, "Utilities & Other Deposit Schedule");
        assert_eq!(s.fm.kind, Kind::Page);
        assert_eq!(s.fm.module.as_deref(), Some("gl"));
        assert_eq!(s.fm.routes, vec!["admin.financial-gl.report.deposit-schedule"]);
        assert_eq!(s.fm.order, 345);
        assert_eq!(s.fm.lang, "en");
        assert!(s.body.starts_with("# Utilities"));
        assert_eq!(
            s.fm.extra.get("aros").map(String::as_str),
            Some("[financial_admin/deposit_schedule]")
        );
    }

    #[test]
    fn page_needs_exactly_one_route() {
        let t = SAMPLE.replace("routes: [admin.financial-gl.report.deposit-schedule]", "routes: [a, b]");
        let err = Source::parse(Path::new("guide/en/gl/x.md"), &t).unwrap_err();
        assert!(err.contains("a page guide needs exactly one route"), "{err}");
    }

    #[test]
    fn missing_front_matter_is_error() {
        assert!(Source::parse(Path::new("guide/en/x.md"), "# no header").is_err());
    }

    #[test]
    fn roundtrip_render_parse() {
        let s = Source::parse(Path::new("guide/en/gl/deposit.md"), SAMPLE).unwrap();
        let again = Source::parse(Path::new("guide/en/gl/deposit.md"), &s.render_text()).unwrap();
        assert_eq!(s, again);
    }

    #[test]
    fn hash_in_title_survives_roundtrip() {
        let s = Source::parse(Path::new("system/x.md"), "---\ntitle: \"Order #5\"\n---\nx\n").unwrap();
        assert_eq!(s.fm.title, "Order #5");
        let again = Source::parse(Path::new("system/x.md"), &s.render_text()).unwrap();
        assert_eq!(again.fm.title, "Order #5");
    }

    #[test]
    fn guide_path_slug() {
        assert_eq!(
            guide_path("id", "", "gl", "admin.gl.deposit"),
            PathBuf::from("guide/id/gl/admin-gl-deposit.md")
        );
        assert_eq!(
            guide_path("en", "", "orders", "/shop/orders/:id"),
            PathBuf::from("guide/en/orders/shop-orders-id.md")
        );
        assert_eq!(
            guide_path("en", "tenant", "bills", "selfcare.bills"),
            PathBuf::from("guide/en/tenant/bills/selfcare-bills.md")
        );
        assert_eq!(slug("anpManagement"), "anp-management");
        assert_eq!(slug("financialGL"), "financial-gl");
    }

    #[test]
    fn body_hash_ignores_trailing_whitespace() {
        assert_eq!(body_hash("a\n"), body_hash("a\n\n  "));
        assert_ne!(body_hash("a"), body_hash("b"));
    }
}
