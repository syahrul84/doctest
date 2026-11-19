//! Checks one source before it is stored: UI names in a page guide must be on the page; links and images must exist;
//! unverified setup steps are flagged.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::config::Config;
use crate::pages::PageFacts;
use crate::source::{Kind, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub level: Level,
    pub message: String,
}

impl Problem {
    fn error(message: String) -> Problem {
        Problem {
            level: Level::Error,
            message,
        }
    }
    fn warning(message: String) -> Problem {
        Problem {
            level: Level::Warning,
            message,
        }
    }
}

/// Bold phrases the guide template itself uses (any language guide may use the English ones; translations add
/// theirs through [guide] allow_bold).
const TEMPLATE_BOLD: [&str; 8] = [
    "What you should see",
    "Note",
    "Tip",
    "Warning",
    "Important",
    "Before you start",
    "Required",
    "Optional",
];

static RAW_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)<(?:a\s[^>]*href|img\s[^>]*src)\s*="#).unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*\n]+?)\*\*").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(!?)\[[^\]]*\]\(([^)\s]+)(?:\s+[^)]*)?\)").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?```|`[^`\n]*`").unwrap());

/// Lowercase, single spaces, without trailing `:` `.` `*` `…`.
pub fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches([':', '.', '*', '…', ' '])
        .to_lowercase()
}

pub fn check_source(src: &Source, facts: Option<&PageFacts>, cfg: &Config, all: &[Source]) -> Vec<Problem> {
    let mut out = Vec::new();
    let text = FENCE.replace_all(&src.body, "");
    if src.fm.kind == Kind::Page
        && let Some(f) = facts
    {
        let known: Vec<String> = f
            .labels
            .get(&src.fm.lang)
            .into_iter()
            .flatten()
            .chain(&f.literals)
            .chain(&f.menu_titles)
            .chain(&cfg.guide.allow_bold)
            .map(|s| norm(s))
            .chain(TEMPLATE_BOLD.iter().map(|s| norm(s)))
            // form fields are labelled after their name (`code` → Code, `area_size` → Area size)
            .chain(
                f.endpoints
                    .iter()
                    .flat_map(|e| e.rules.keys())
                    .map(|k| norm(&crate::pages::headline(k.split('.').next_back().unwrap_or(k)))),
            )
            .collect();
        let mut reported = Vec::new();
        for cap in BOLD.captures_iter(&text) {
            // a menu path is written **A > B**; a label may itself hold ">" or "->"
            let whole = norm(&cap[1]);
            let parts: Vec<&str> = if known.contains(&whole) || !cap[1].contains(" > ") && !cap[1].contains(" › ") {
                vec![&cap[1]]
            } else {
                cap[1].split(" > ").flat_map(|p| p.split(" › ")).collect()
            };
            for part in parts {
                let n = norm(part);
                if n.is_empty() || known.contains(&n) || reported.contains(&n) {
                    continue;
                }
                reported.push(n);
                out.push(Problem::error(format!(
                    "unknown UI name \"{}\" on {}; use the on-screen text or add it to [guide] allow_bold",
                    part.trim().trim_end_matches(':'),
                    f.page.key
                )));
            }
        }
    }
    if src.fm.kind == Kind::Page
        && let Some(f) = facts
    {
        let acts = crate::pages::actions(f);
        let headings: Vec<String> = src
            .body
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(|l| l.to_lowercase())
            .collect();
        if cfg.guide.require_actions && src.fm.lang == cfg.primary_language() {
            for a in &acts {
                let words: &[&str] = match a.name.as_str() {
                    "Add" => &["add", "create", "new"],
                    "Edit" => &["edit", "update", "change"],
                    "Delete" => &["delete", "remove"],
                    _ => &[],
                };
                let name = a.name.to_lowercase();
                let found = headings
                    .iter()
                    .any(|h| h.contains(&name) || words.iter().any(|w| h.contains(w)));
                if !found {
                    out.push(Problem::error(format!(
                        "no section for the {} action ({}): add a heading such as \"## {} …\" with the steps and what the user should see after it",
                        a.name, a.endpoint, a.name
                    )));
                }
                // only messages that read as sentences (an untranslated key cannot be quoted)
                for m in a.success.iter().filter(|m| m.trim().contains(' ')) {
                    if !src.body.contains(m.as_str()) {
                        out.push(Problem::warning(format!(
                            "the {} action's success message *{m}* is not in the guide",
                            a.name
                        )));
                    }
                }
            }
        }
        let no_data = crate::pages::says_no_data(&src.body);
        let min = cfg
            .guide
            .min_screenshots
            .unwrap_or_else(|| crate::pages::screenshot_plan_for(&acts, no_data).len());
        let images = LINK.captures_iter(&text).filter(|c| &c[1] == "!").count();
        if images < min {
            // text first: screenshots come after (shoot_pages takes the page shot, a shots:<page> item the rest)
            out.push(Problem::warning(format!(
                "{images} of {min} screenshots so far (the page, one form shot for Add and Edit, one per other action) — \
                 fine while writing: shoot_pages adds the page shot and a shots:{} item asks for the rest{}",
                src.fm.routes.first().map(String::as_str).unwrap_or("<page>"),
                if no_data { "" } else { ". If the page has no records to show, put <!-- no-data: why --> in the guide and fewer are needed" }
            )));
        }
    }
    let dir = src.path.parent().unwrap_or(Path::new(""));
    for cap in LINK.captures_iter(&text) {
        let target = &cap[2];
        if target.starts_with("http:")
            || target.starts_with("https:")
            || target.starts_with('#')
            || target.starts_with("mailto:")
        {
            continue;
        }
        let path = target.split('#').next().unwrap_or_default();
        let in_guide = src.path.starts_with("guide");
        if in_guide && &cap[1] != "!" {
            let resolved = if path.starts_with("guide/") {
                PathBuf::from(path)
            } else {
                normalize(&dir.join(path))
            };
            if !resolved.starts_with("guide") || path.starts_with("assets/") {
                out.push(Problem::error(format!(
                    "link to {target}: the guide is published alone (at {}), so it cannot link to the start page, the system document or their assets",
                    cfg.guide.publish_path
                )));
                continue;
            }
            if path.ends_with(".md") {
                if !all.iter().any(|s| s.path == resolved) {
                    out.push(Problem::error(format!("link to {target} does not match any source")));
                }
                continue;
            }
        }
        if in_guide && &cap[1] == "!" && !path.starts_with("guide/img/") {
            out.push(Problem::error(format!(
                "image {target}: published guide pages take images from guide/img/<lang>/… only"
            )));
            continue;
        }
        if in_guide && &cap[1] != "!" && path.ends_with(".md") && !normalize(&dir.join(path)).starts_with("guide") {
            out.push(Problem::error(format!(
                "link to {target}: the guide is published alone (at {}), so it cannot link to the start page or the system document",
                cfg.guide.publish_path
            )));
            continue;
        }
        if &cap[1] == "!" {
            let webp = path.strip_suffix(".png").map(|p| format!("{p}.webp"));
            if let Some(w) = webp.filter(|w| !cfg.out.join(path).is_file() && cfg.out.join(w).is_file()) {
                out.push(Problem::error(format!(
                    "image {target}: screenshots are WebP now; use {w} (`doctest shrink` updates every guide)"
                )));
            } else if !cfg.out.join(path).is_file() {
                out.push(Problem::error(format!("image {target} does not exist (images live under the document folder, e.g. guide/img/<lang>/<id>.webp)")));
            }
        } else if path.ends_with(".md") {
            let resolved = normalize(&dir.join(path));
            if !all.iter().any(|s| s.path == resolved) {
                out.push(Problem::error(format!("link to {target} does not match any source")));
            }
        }
    }
    if src.path.starts_with("guide") && RAW_LINK.is_match(&text) {
        out.push(Problem::error("raw HTML links or images (<a href>, <img src>) are not checked: use Markdown [text](path) and ![alt](guide/img/…)".into()));
    }
    let unverified = src.body.matches("<!-- unverified -->").count();
    if unverified > 0 {
        out.push(Problem::warning(format!(
            "{}: {unverified} step(s) marked unverified",
            src.path.display()
        )));
    } else if src.fm.verified == Some(false) {
        out.push(Problem::warning(format!(
            "{}: marked verified: false",
            src.path.display()
        )));
    }
    out
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::tests::shop_config;
    use crate::pages::{Page, PageFacts};
    use std::collections::BTreeMap;

    fn facts() -> PageFacts {
        PageFacts {
            page: Page {
                key: "shop.orders.create".into(),
                app: "web".into(),
                path: "/shop/orders/create".into(),
                route_name: Some("shop.orders.create".into()),
                component: "a.vue".into(),
                module: "orders".into(),
                portal: String::new(),
                stack: String::new(),
            },
            labels: BTreeMap::from([
                ("en".to_string(), vec!["Tenant".to_string(), "Email".to_string()]),
                ("id".to_string(), vec!["Penyewa".to_string(), "Simpan".to_string()]),
            ]),
            literals: vec!["Save".into()],
            components: vec![],
            code_files: vec![],
            endpoints: vec![],
            menu_titles: vec!["Orders".into(), "New Order".into()],
        }
    }

    fn page(lang: &str, body: &str) -> Source {
        let text =
            format!("---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\n---\n{body}\n");
        Source::parse(
            &PathBuf::from(format!("guide/{lang}/orders/shop-orders-create.md")),
            &text,
        )
        .unwrap()
    }

    fn run(src: &Source) -> Vec<Problem> {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        check_source(src, Some(&facts()), &cfg, std::slice::from_ref(src))
    }

    #[test]
    fn known_label_ok() {
        assert_eq!(
            run(&page("en", "Click **Save**. Enter the **Tenant** and **email:**")),
            vec![]
        );
    }

    #[test]
    fn menu_path_ok() {
        assert_eq!(
            run(&page(
                "en",
                "Open **Orders** > **New Order**, or **Orders > New Order**."
            )),
            vec![]
        );
    }

    #[test]
    fn labels_with_arrows_and_field_names() {
        let src = page(
            "en",
            "Set **Header rows -> 2**, then fill **Code** and **Area size**. Open **Orders > New Order**.",
        );
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let mut f = facts();
        f.literals.push("Header rows -> 2".into());
        f.endpoints.push(crate::pages::Endpoint {
            method: "POST".into(),
            uri: "api/locations".into(),
            rules: BTreeMap::from([
                ("code".to_string(), vec!["required".to_string()]),
                ("details.area_size".to_string(), vec![]),
            ]),
            ..Default::default()
        });
        let p = check_source(&src, Some(&f), &cfg, std::slice::from_ref(&src));
        assert!(p.iter().all(|x| !x.message.contains("unknown UI name")), "{p:?}");
    }

    #[test]
    fn unknown_name_error() {
        let p = run(&page("en", "Click **Submit**."));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].level, Level::Error);
        assert!(p[0].message.contains("unknown UI name \"Submit\""), "{}", p[0].message);
    }

    #[test]
    fn allow_bold_ok() {
        assert_eq!(run(&page("en", "**Note:** read this.")), vec![]);
    }

    #[test]
    fn template_phrases_are_allowed() {
        assert_eq!(
            run(&page(
                "en",
                "**What you should see:** saved. **Tip:** x. **Warning:** y."
            )),
            vec![]
        );
    }

    #[test]
    fn label_checked_in_guide_language() {
        assert_eq!(run(&page("id", "Klik **Simpan**.")), vec![]);
        assert_eq!(run(&page("id", "Klik **Tenant**.")).len(), 1);
    }

    #[test]
    fn broken_link_and_missing_image() {
        let p = run(&page(
            "en",
            "See [other](../billing/invoice.md) and ![shot](guide/img/en/orders/x-1.png). [web](https://x.y) [top](#a)",
        ));
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p.iter().any(|x| x.message.contains("link to ../billing/invoice.md")));
        assert!(
            p.iter()
                .any(|x| x.message.contains("image guide/img/en/orders/x-1.png"))
        );
    }

    fn strict_facts() -> PageFacts {
        let mut f = facts();
        f.endpoints = vec![
            crate::pages::Endpoint {
                method: "POST".into(),
                uri: "api/orders".into(),
                messages: vec!["Order saved.".into()],
                ..Default::default()
            },
            crate::pages::Endpoint {
                method: "DELETE".into(),
                uri: "api/orders/{order}".into(),
                ..Default::default()
            },
        ];
        f
    }

    fn strict_run(body: &str) -> Vec<Problem> {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = shop_config(d.path());
        cfg.guide.require_actions = true;
        cfg.guide.min_screenshots = None;
        for i in 1..=3 {
            let p = cfg.out.join(format!("guide/img/en/orders/shop-orders-create-{i}.png"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "png").unwrap();
        }
        let src = page("en", body);
        check_source(&src, Some(&strict_facts()), &cfg, std::slice::from_ref(&src))
    }

    #[test]
    fn page_guide_needs_a_section_per_action_and_screenshots() {
        let p = strict_run("Click **Save**.");
        let msgs: Vec<&str> = p
            .iter()
            .filter(|x| x.level == Level::Error)
            .map(|x| x.message.as_str())
            .collect();
        assert!(
            msgs.iter().any(|m| m.contains("no section for the Add action")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("no section for the Delete action")),
            "{msgs:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.level == Level::Warning && x.message.contains("0 of 3 screenshots")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.level == Level::Warning && x.message.contains("Order saved.")),
            "{p:?}"
        );
    }

    #[test]
    fn complete_page_guide_passes() {
        let body = "Open **Orders**.\n\n![list](guide/img/en/orders/shop-orders-create-1.png)\n\n## Add an order\n1. Click **Save**.\n\nYou see *Order saved.*\n\n![form](guide/img/en/orders/shop-orders-create-2.png)\n\n## Delete an order\nConfirm.\n\n![confirm](guide/img/en/orders/shop-orders-create-3.png)\n";
        let p = strict_run(body);
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn published_guides_do_not_link_inside() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let api = Source::parse(Path::new("system/api.md"), "---\ntitle: API\n---\nx\n").unwrap();
        let g = page("en", "See [the API](../../../system/api.md) and ![x](assets/logo.png).");
        let p = check_source(&g, None, &cfg, &[g.clone(), api]);
        assert!(
            p.iter()
                .any(|x| x.message.contains("published alone") && x.message.contains("system/api.md")),
            "{p:?}"
        );
        assert!(p.iter().any(|x| x.message.contains("guide/img")), "{p:?}");
    }

    #[test]
    fn published_guides_keep_every_link_inside() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let other = page("en", "x");
        let mut other = other;
        other.path = PathBuf::from("guide/en/orders/other.md");
        let g = page(
            "en",
            "[pdf](assets/manual.pdf) [api](../../../system/api.html) <a href=\"../../../index.html\">x</a> [root](guide/en/orders/other.md)",
        );
        let p = check_source(&g, None, &cfg, &[g.clone(), other]);
        let msgs: Vec<&str> = p.iter().map(|x| x.message.as_str()).collect();
        assert!(msgs.iter().any(|m| m.contains("assets/manual.pdf")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("system/api.html")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("HTML")), "{msgs:?}");
        assert!(
            !msgs.iter().any(|m| m.contains("guide/en/orders/other.md")),
            "root links into guide/ are fine: {msgs:?}"
        );
    }

    #[test]
    fn unverified_steps_warn_in_system_docs() {
        let s = Source::parse(
            Path::new("index.md"),
            "---\ntitle: Install\n---\nRun it.\n<!-- unverified -->\nRestart nginx.\n",
        )
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let p = check_source(&s, None, &cfg, std::slice::from_ref(&s));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].level, Level::Warning);
    }
}
