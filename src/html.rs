//! Builds the static site under `document/` from the sources: start page, system document, user guide per
//! language, change log, navigation and a client-side search index. Links are relative, so it works from file://.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
use serde_json::json;

use crate::config::Config;
use crate::source::{Kind, Source};
use crate::state::State;

pub const HEADER_PREFIX: &str = "<!-- Built by doctest";
const CSS: &str = include_str!("../assets/doctest.css");
const JS: &str = include_str!("../assets/doctest.js");

#[derive(Debug, Default)]
pub struct BuildReport {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn out_path(src: &Path) -> PathBuf {
    src.with_extension("html")
}

/// `../` once per folder level of `rel`.
fn root_of(rel: &Path) -> String {
    "../".repeat(rel.components().count().saturating_sub(1))
}

/// Path from the folder of `from` to `to` (both document-relative).
fn relative(from: &Path, to: &Path) -> String {
    let fd: Vec<_> = from.parent().map(|p| p.components().collect()).unwrap_or_default();
    let td: Vec<_> = to.components().collect();
    let common = fd.iter().zip(td.iter()).take_while(|(a, b)| a == b).count();
    let mut s = "../".repeat(fd.len() - common);
    s += &td[common..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    s
}

/// Wraps the block after each `<!-- unverified -->` marker in an Unverified box.
fn mark_unverified(body: &str) -> String {
    let mut out = String::new();
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "<!-- unverified -->" {
            out += "<div class=\"unverified\"><span class=\"badge\">Unverified</span>\n\n";
            while let Some(l) = lines.peek() {
                if l.trim().is_empty() {
                    break;
                }
                out += lines.next().unwrap_or_default();
                out.push('\n');
            }
            out += "\n</div>\n";
        } else {
            out += line;
            out.push('\n');
        }
    }
    out
}

/// Markdown → HTML for the page at `rel`: `.md` links become `.html`, document-root image paths become relative.
fn render_markdown(body: &str, rel: &Path) -> String {
    let text = mark_unverified(body);
    let parser = Parser::new_ext(&text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(|ev| match ev {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let d = dest_url.to_string();
            let fixed = if d.contains("://") || d.starts_with('#') || d.starts_with("mailto:") {
                d
            } else {
                let (path, anchor) = d
                    .split_once('#')
                    .map(|(p, a)| (p.to_string(), format!("#{a}")))
                    .unwrap_or((d.clone(), String::new()));
                let path = match path.strip_suffix(".md") {
                    Some(p) => format!("{p}.html"),
                    None => path,
                };
                let path = if path.starts_with("assets/") || path.starts_with("guide/") {
                    relative(rel, Path::new(&path))
                } else {
                    path
                };
                format!("{path}{anchor}")
            };
            Event::Start(Tag::Link {
                link_type,
                dest_url: CowStr::from(fixed),
                title,
                id,
            })
        }
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let d = dest_url.to_string();
            let fixed = if d.starts_with("assets/") || d.starts_with("guide/") {
                relative(rel, Path::new(&d))
            } else {
                d
            };
            Event::Start(Tag::Image {
                link_type,
                dest_url: CowStr::from(fixed),
                title,
                id,
            })
        }
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

fn plain(body: &str) -> String {
    let mut s = String::new();
    for ev in Parser::new_ext(body, Options::ENABLE_TABLES) {
        if let Event::Text(t) | Event::Code(t) = ev {
            s += &t;
            s.push(' ');
        }
    }
    s.chars().take(2000).collect()
}

struct Site<'a> {
    cfg: &'a Config,
    system: Vec<&'a Source>,
    has_index: bool,
    guide_langs: Vec<String>,
}

impl Site<'_> {
    #[allow(clippy::too_many_arguments)]
    fn page(
        &self,
        rel: &Path,
        source: Option<&Path>,
        title: &str,
        lang: Option<&str>,
        langs_html: &str,
        nav_extra: &str,
        content: &str,
    ) -> String {
        let root = root_of(rel);
        let header = match source {
            Some(s) => format!(
                "{HEADER_PREFIX} from _source/{}; edit the source, not this file -->",
                s.display()
            ),
            None => format!("{HEADER_PREFIX} (generated); do not edit -->"),
        };
        let here = |p: &str| if Path::new(p) == rel { " class=\"here\"" } else { "" };
        let mut nav = format!(
            "<h4>Start</h4><ul><li><a{} href=\"{root}index.html\">Start here</a></li></ul>",
            here("index.html")
        );
        if !self.system.is_empty() {
            nav += "<h4>System</h4><ul>";
            for s in &self.system {
                let p = out_path(&s.path);
                nav += &format!(
                    "<li><a{} href=\"{root}{}\">{}</a></li>",
                    here(&p.to_string_lossy()),
                    p.display(),
                    esc(&s.fm.title)
                );
            }
            nav += &format!(
                "<li><a{} href=\"{root}system/changes.html\">Changes</a></li></ul>",
                here("system/changes.html")
            );
        }
        if !self.guide_langs.is_empty() {
            let _ = lang;
            nav += &format!(
                "<h4>User guide</h4><ul><li><a href=\"{root}guide/index.html\">User guide (published at {})</a></li></ul>",
                esc(&self.cfg.guide.publish_path)
            );
        }
        nav += nav_extra;
        format!(
            "{header}\n<!doctype html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{t} · {name}</title>\n<link rel=\"stylesheet\" href=\"{root}assets/doctest.css\">\n</head>\n<body data-root=\"{root}\" data-lang=\"{lang}\">\n<header class=\"top\"><a class=\"brand\" href=\"{root}index.html\">{name}</a><div id=\"search\"><input type=\"search\" placeholder=\"Search\" aria-label=\"Search\"><div id=\"results\"></div></div><div class=\"langs\">{langs_html}</div></header>\n<div class=\"layout\">\n<nav class=\"side\">{nav}</nav>\n<main>\n{content}\n<footer>Generated by doctest.</footer>\n</main>\n</div>\n<script src=\"{root}assets/search.js\"></script>\n<script src=\"{root}assets/doctest.js\"></script>\n</body>\n</html>\n",
            lang = esc(lang.unwrap_or(self.cfg.primary_language())),
            t = esc(title),
            name = esc(&self.cfg.name),
        )
    }
}

/// A page of the published guide: everything it links to stays inside `guide/`.
#[allow(clippy::too_many_arguments)]
fn guide_page(
    cfg: &Config,
    rel: &Path,
    source: Option<&Path>,
    title: &str,
    lang: &str,
    langs_html: &str,
    nav: &str,
    content: &str,
) -> String {
    // rel is guide/…; the guide root is one level up per folder below guide/
    let groot = "../".repeat(rel.components().count().saturating_sub(2));
    let header = match source {
        Some(s) => format!(
            "{HEADER_PREFIX} from _source/{}; edit the source, not this file -->",
            s.display()
        ),
        None => format!("{HEADER_PREFIX} (generated); do not edit -->"),
    };
    format!(
        "{header}\n<!doctype html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{t} · {name}</title>\n<link rel=\"stylesheet\" href=\"{groot}assets/doctest.css\">\n</head>\n<body data-root=\"{groot}\" data-lang=\"{lang}\">\n<header class=\"top\"><a class=\"brand\" href=\"{groot}index.html\">{name} — user guide</a><div id=\"search\"><input type=\"search\" placeholder=\"Search\" aria-label=\"Search\"><div id=\"results\"></div></div><div class=\"langs\">{langs_html}</div></header>\n<div class=\"layout\">\n<nav class=\"side\"><ul><li><a href=\"{groot}index.html\">All guides</a></li></ul>{nav}</nav>\n<main>\n{content}\n<footer>Generated by doctest.</footer>\n</main>\n</div>\n<script src=\"{groot}assets/search.js\"></script>\n<script src=\"{groot}assets/doctest.js\"></script>\n</body>\n</html>\n",
        lang = esc(lang),
        t = esc(title),
        name = esc(&cfg.name),
    )
}

/// `guide/<lang>/<portal>/…` → portal ("" without portals).
fn portal_of_path(cfg: &Config, path: &Path) -> String {
    if cfg.guide.portals.is_empty() {
        return String::new();
    }
    let third = path
        .components()
        .nth(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default();
    let known = cfg.guide.portals.iter().any(|p| crate::source::slug(&p.name) == third) || third == "other";
    if known && path.components().count() > 4 {
        third
    } else {
        String::new()
    }
}

fn portal_title(cfg: &Config, portal: &str) -> String {
    cfg.guide
        .portals
        .iter()
        .find(|p| p.name == portal)
        .map(|p| p.title.clone())
        .unwrap_or_else(|| {
            if portal == "other" {
                "Other pages".into()
            } else {
                portal.to_string()
            }
        })
}

/// `guide/<lang>/[<portal>/]index.html`
fn guide_index_path(lang: &str, portal: &str) -> PathBuf {
    if portal.is_empty() {
        PathBuf::from(format!("guide/{lang}/index.html"))
    } else {
        PathBuf::from(format!("guide/{lang}/{portal}/index.html"))
    }
}

fn write(cfg: &Config, rel: &Path, text: &str, report: &mut BuildReport) -> Result<(), String> {
    let path = cfg.out.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    report.written.push(rel.to_path_buf());
    Ok(())
}

pub fn build(cfg: &Config, sources: &[Source], state: &State) -> Result<BuildReport, String> {
    // parallel workers: one build at a time, and from the sources on disk now (another worker may have added one)
    let _lock = crate::state::FileLock::acquire(cfg.out.join(".doctest/build.lock"))?;
    let fresh;
    let sources = if crate::source::source_root(cfg).is_dir() {
        fresh = crate::source::sources(cfg).unwrap_or_else(|_| sources.to_vec());
        let mut merged = fresh.clone();
        for s in sources {
            if !merged.iter().any(|m| m.path == s.path) {
                merged.push(s.clone());
            }
        }
        merged
    } else {
        sources.to_vec()
    };
    let sources = &sources[..];
    let mut report = BuildReport::default();
    let mut system: Vec<&Source> = sources.iter().filter(|s| s.path.starts_with("system")).collect();
    system.sort_by(|a, b| (a.fm.order, &a.fm.title).cmp(&(b.fm.order, &b.fm.title)));
    let guides: Vec<&Source> = sources.iter().filter(|s| s.path.starts_with("guide")).collect();
    let mut guide_langs: Vec<String> = cfg
        .languages
        .iter()
        .filter(|l| guides.iter().any(|g| &g.fm.lang == *l))
        .cloned()
        .collect();
    for g in &guides {
        if !guide_langs.contains(&g.fm.lang) {
            guide_langs.push(g.fm.lang.clone());
        }
    }
    let site = Site {
        cfg,
        system: system.clone(),
        has_index: sources.iter().any(|s| s.path == Path::new("index.md")),
        guide_langs: guide_langs.clone(),
    };
    let mut search = Vec::new();

    // guides by (language, portal) → module → guides
    let mut by_lang: BTreeMap<(String, String), Vec<&Source>> = BTreeMap::new();
    for g in &guides {
        by_lang
            .entry((g.fm.lang.clone(), portal_of_path(cfg, &g.path)))
            .or_default()
            .push(g);
    }
    let mut guide_search = Vec::new();
    let modules_of = |list: &[&Source]| -> Vec<(String, Vec<Source>)> {
        let mut m: BTreeMap<String, Vec<Source>> = BTreeMap::new();
        for g in list {
            m.entry(g.fm.module.clone().unwrap_or_else(|| "general".into()))
                .or_default()
                .push((*g).clone());
        }
        let mut v: Vec<(String, Vec<Source>)> = m.into_iter().collect();
        for (_, gs) in v.iter_mut() {
            gs.sort_by(|a, b| (a.fm.order, &a.fm.title).cmp(&(b.fm.order, &b.fm.title)));
        }
        v.sort_by_key(|(name, gs)| (gs.iter().map(|g| g.fm.order).min().unwrap_or(0), name.clone()));
        v
    };

    for s in sources {
        let rel = out_path(&s.path);
        let content = render_markdown(&s.body, &rel);
        let (lang, langs_html, nav_extra) = if s.path.starts_with("guide") {
            let lang = s.fm.lang.as_str();
            let mut langs = String::new();
            if guide_langs.len() > 1 {
                for l in &guide_langs {
                    let mut other: Vec<_> = s
                        .path
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect();
                    if other.len() > 1 {
                        other[1] = l.clone();
                    }
                    let other_src = PathBuf::from(other.join("/"));
                    if l == lang {
                        langs += &format!("<a class=\"on\">{}</a>", esc(l));
                    } else if sources.iter().any(|x| x.path == other_src) {
                        langs += &format!("<a href=\"{}\">{}</a>", relative(&rel, &out_path(&other_src)), esc(l));
                    }
                }
            }
            let portal = portal_of_path(cfg, &s.path);
            let mut nav = String::new();
            if !portal.is_empty() {
                nav += &format!(
                    "<h4>{}</h4><ul><li><a href=\"{}\">Contents</a></li></ul>",
                    esc(&portal_title(cfg, &portal)),
                    relative(&rel, &guide_index_path(lang, &portal))
                );
            }
            for (module, gs) in modules_of(
                by_lang
                    .get(&(lang.to_string(), portal.clone()))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            ) {
                nav += &format!("<h4>{}</h4><ul>", esc(&module));
                for g in gs {
                    let p = out_path(&g.path);
                    let cls = if p == rel { " class=\"here\"" } else { "" };
                    nav += &format!(
                        "<li><a{cls} href=\"{}\">{}</a></li>",
                        relative(&rel, &p),
                        esc(&g.fm.title)
                    );
                }
                nav += "</ul>";
            }
            (Some(lang), langs, nav)
        } else {
            (None, String::new(), String::new())
        };
        if let Some(lang) = lang {
            let html = guide_page(
                cfg,
                &rel,
                Some(&s.path),
                &s.fm.title,
                lang,
                &langs_html,
                &nav_extra,
                &content,
            );
            write(cfg, &rel, &html, &mut report)?;
            let url = rel
                .strip_prefix("guide")
                .unwrap_or(&rel)
                .to_string_lossy()
                .replace('\\', "/");
            guide_search.push(json!({"title": s.fm.title, "url": url, "lang": lang, "text": plain(&s.body)}));
        } else {
            let html = site.page(
                &rel,
                Some(&s.path),
                &s.fm.title,
                None,
                &langs_html,
                &nav_extra,
                &content,
            );
            write(cfg, &rel, &html, &mut report)?;
            search.push(json!({"title": s.fm.title, "url": rel.to_string_lossy(), "lang": "", "text": plain(&s.body)}));
        }
    }

    if !site.has_index {
        let content = format!(
            "<h1>{}</h1>\n<p>The start page has not been written yet. doctest lists it as <code>doc:index</code>.</p>",
            esc(&cfg.name)
        );
        write(
            cfg,
            Path::new("index.html"),
            &site.page(Path::new("index.html"), None, "Start here", None, "", "", &content),
            &mut report,
        )?;
    }

    for ((lang, portal), list) in &by_lang {
        let rel = guide_index_path(lang, portal);
        let title = if portal.is_empty() {
            "User guide".to_string()
        } else {
            portal_title(cfg, portal)
        };
        let mut content = format!("<h1>{}</h1>\n", esc(&title));
        for (module, gs) in modules_of(list) {
            content += &format!("<h2>{}</h2>\n<ul class=\"guide-list\">", esc(&module));
            for g in gs {
                let kind = if g.fm.kind == Kind::Process { " (process)" } else { "" };
                content += &format!(
                    "<li><a href=\"{}\">{}</a>{kind}<br><small>{}</small></li>",
                    relative(&rel, &out_path(&g.path)),
                    esc(&g.fm.title),
                    esc(&g.fm.summary)
                );
            }
            content += "</ul>\n";
        }
        write(
            cfg,
            &rel,
            &guide_page(cfg, &rel, None, &title, lang, "", "", &content),
            &mut report,
        )?;
    }
    if !guides.is_empty() {
        let rel = PathBuf::from("guide/index.html");
        let mut content = format!("<h1>{} — user guide</h1>\n", esc(&cfg.name));
        for lang in &guide_langs {
            content += &format!("<h2>{}</h2>\n<ul class=\"guide-list\">", esc(lang));
            for (l, portal) in by_lang.keys().filter(|(l, _)| l == lang) {
                let title = if portal.is_empty() {
                    "User guide".to_string()
                } else {
                    portal_title(cfg, portal)
                };
                content += &format!(
                    "<li><a href=\"{}\">{}</a></li>",
                    relative(&rel, &guide_index_path(l, portal)),
                    esc(&title)
                );
            }
            content += "</ul>\n";
        }
        write(
            cfg,
            &rel,
            &guide_page(cfg, &rel, None, "User guide", cfg.primary_language(), "", "", &content),
            &mut report,
        )?;
        write(cfg, Path::new("guide/assets/doctest.css"), CSS, &mut report)?;
        write(cfg, Path::new("guide/assets/doctest.js"), JS, &mut report)?;
        let gindex = serde_json::to_string(&guide_search).map_err(|e| e.to_string())?;
        write(
            cfg,
            Path::new("guide/assets/search.js"),
            &format!("window.DOCTEST_INDEX = {gindex};\n"),
            &mut report,
        )?;
    }

    let rel = PathBuf::from("system/changes.html");
    let mut content = "<h1>Changes</h1>\n".to_string();
    if state.runs.is_empty() {
        content += "<p>No finished doctest runs yet.</p>";
    }
    for r in state.runs.iter().rev() {
        let commits: Vec<String> = r
            .commits
            .iter()
            .map(|(k, v)| {
                format!(
                    "{} <code>{}</code>",
                    esc(k),
                    esc(&v.chars().take(10).collect::<String>())
                )
            })
            .collect();
        let took = r
            .seconds
            .map(|s| format!(" — took {}", crate::work::duration(s)))
            .unwrap_or_default();
        content += &format!(
            "<h2>{}{}</h2>\n<p>{}</p>\n<p>{}</p>\n<ul>",
            esc(&r.at),
            esc(&took),
            commits.join(", "),
            esc(&r.note)
        );
        for u in &r.updated {
            content += &format!("<li><code>{}</code></li>", esc(u));
        }
        content += "</ul>\n";
    }
    write(
        cfg,
        &rel,
        &site.page(&rel, None, "Changes", None, "", "", &content),
        &mut report,
    )?;

    write(cfg, Path::new("assets/doctest.css"), CSS, &mut report)?;
    write(cfg, Path::new("assets/doctest.js"), JS, &mut report)?;
    let index = serde_json::to_string(&search).map_err(|e| e.to_string())?;
    write(
        cfg,
        Path::new("assets/search.js"),
        &format!("window.DOCTEST_INDEX = {index};\n"),
        &mut report,
    )?;

    remove_orphans(cfg, &cfg.out, &mut report);
    Ok(report)
}

fn remove_orphans(cfg: &Config, dir: &Path, report: &mut BuildReport) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if name != "_source" && name != ".doctest" && name != "assets" && name != "img" {
                remove_orphans(cfg, &p, report);
            }
            continue;
        }
        if p.extension().is_none_or(|x| x != "html") {
            continue;
        }
        let rel = p.strip_prefix(&cfg.out).unwrap_or(&p).to_path_buf();
        if report.written.contains(&rel) {
            continue;
        }
        let ours = std::fs::read_to_string(&p)
            .map(|t| t.starts_with(HEADER_PREFIX))
            .unwrap_or(false);
        if ours && std::fs::remove_file(&p).is_ok() {
            report.removed.push(rel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::tests::shop_config;
    use crate::source::Source;
    use std::fs;

    fn src(path: &str, text: &str) -> Source {
        Source::parse(Path::new(path), text).unwrap()
    }

    fn sample() -> Vec<Source> {
        vec![
            src(
                "index.md",
                "---\ntitle: Start here\n---\n# Shop\n\nRead the [guide](guide/en/index.md).\n\n<!-- unverified -->\nRestart php-fpm after deploy.\n",
            ),
            src(
                "system/configuration.md",
                "---\ntitle: Configuration\norder: 2\n---\nKeys.\n",
            ),
            src(
                "guide/en/orders/shop-orders-create.md",
                "---\ntitle: New Order\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\norder: 1\nsummary: Create an order.\n---\nClick **Save**. See [list](shop-orders-index.md).\n\n![form](guide/img/en/orders/shop-orders-create-1.png)\n",
            ),
            src(
                "guide/en/orders/shop-orders-index.md",
                "---\ntitle: Order List\nkind: page\nmodule: orders\nroutes: [shop.orders.index]\norder: 2\n---\nThe list.\n",
            ),
            src(
                "guide/id/orders/shop-orders-create.md",
                "---\ntitle: Pesanan Baru\nkind: page\nmodule: orders\nroutes: [shop.orders.create]\norder: 1\n---\nKlik **Simpan**.\n",
            ),
        ]
    }

    fn built() -> (tempfile::TempDir, Config, BuildReport) {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let r = build(&cfg, &sample(), &State::default()).unwrap();
        (d, cfg, r)
    }

    fn read(cfg: &Config, rel: &str) -> String {
        fs::read_to_string(cfg.out.join(rel)).unwrap_or_else(|_| panic!("{rel} not built"))
    }

    #[test]
    fn builds_index_and_guides() {
        let (_d, cfg, _) = built();
        let index = read(&cfg, "index.html");
        assert!(index.starts_with("<!-- Built by doctest from _source/index.md; edit the source, not this file -->"));
        assert!(index.contains("<title>Start here · shop</title>"));
        let en = read(&cfg, "guide/en/orders/shop-orders-create.html");
        assert!(
            en.contains(r#"href="../../id/orders/shop-orders-create.html""#),
            "language switch missing"
        );
        assert!(en.contains("<strong>Save</strong>"));
        assert!(read(&cfg, "guide/id/orders/shop-orders-create.html").contains("Simpan"));
        let gi = read(&cfg, "guide/en/index.html");
        assert!(gi.contains(r#"href="orders/shop-orders-create.html""#) && gi.contains("Create an order."));
        assert!(gi.find("New Order").unwrap() < gi.find("Order List").unwrap());
    }

    #[test]
    fn rewrites_md_links_and_images() {
        let (_d, cfg, _) = built();
        let en = read(&cfg, "guide/en/orders/shop-orders-create.html");
        assert!(en.contains(r#"href="shop-orders-index.html""#));
        assert!(
            en.contains(r#"src="../../img/en/orders/shop-orders-create-1.png""#),
            "{en}"
        );
        assert!(read(&cfg, "index.html").contains(r#"href="guide/en/index.html""#));
    }

    #[test]
    fn unverified_badge() {
        let (_d, cfg, _) = built();
        let index = read(&cfg, "index.html");
        assert!(index.contains(r#"<div class="unverified"><span class="badge">Unverified</span>"#));
        assert!(index.contains("Restart php-fpm"));
    }

    #[test]
    fn removes_orphan_html_only_with_header() {
        let (_d, cfg, _) = built();
        fs::write(cfg.out.join("notes.html"), "<p>mine</p>").unwrap();
        let mut fewer = sample();
        fewer.retain(|s| !s.path.ends_with("shop-orders-index.md"));
        build(&cfg, &fewer, &State::default()).unwrap();
        assert!(cfg.out.join("notes.html").is_file());
        assert!(!cfg.out.join("guide/en/orders/shop-orders-index.html").exists());
    }

    #[test]
    fn search_index_has_all_pages() {
        let (_d, cfg, _) = built();
        let js = read(&cfg, "assets/search.js");
        assert!(js.starts_with("window.DOCTEST_INDEX = "));
        for t in ["Start here", "Configuration"] {
            assert!(js.contains(t), "{t} not in the internal search index");
        }
        let g = read(&cfg, "guide/assets/search.js");
        for t in ["New Order", "Order List", "Pesanan Baru"] {
            assert!(g.contains(t), "{t} not in the guide search index");
        }
    }

    #[test]
    fn changes_page_from_runs() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let mut st = State::default();
        st.runs.push(crate::state::RunRecord {
            at: "2026-11-18T10:00:00Z".into(),
            commits: [("shop".to_string(), "abc1234".to_string())].into(),
            updated: vec!["guide:en:shop.orders.create".into()],
            note: "first".into(),
            seconds: Some(61),
        });
        build(&cfg, &sample(), &st).unwrap();
        let c = read(&cfg, "system/changes.html");
        assert!(c.contains("abc1234") && c.contains("guide:en:shop.orders.create"));
        assert!(c.contains("took 1m 1s"), "{c}");
    }

    fn portal_config(dir: &Path) -> Config {
        let shop = crate::pages::tests::fixtures().join("shop");
        let toml = format!(
            "[project]\nname=\"shop\"\nbackend=\"{b}\"\nfrontends=[\"{b}/web\"]\nlanguages=[\"en\",\"id\"]\n[guide.portals.admin]\ntitle=\"Admin guide\"\npages=[\"admin.*\"]\n[guide.portals.tenant]\ntitle=\"Tenant guide\"\npages=[\"selfcare.*\"]\n",
            b = shop.display()
        );
        std::fs::write(dir.join(".doctest.toml"), toml).unwrap();
        Config::find(dir).unwrap()
    }

    fn portal_sources() -> Vec<Source> {
        vec![
            src("index.md", "---\ntitle: Start here\n---\nInternal setup.\n"),
            src("system/configuration.md", "---\ntitle: Configuration\n---\nKeys.\n"),
            src(
                "guide/en/admin/orders/admin-orders.md",
                "---\ntitle: Orders\nkind: page\nmodule: orders\nroutes: [admin.orders]\n---\nAdmin text.\n\n![list](guide/img/en/orders/admin-orders-1.png)\n",
            ),
            src(
                "guide/en/tenant/bills/selfcare-bills.md",
                "---\ntitle: My Bills\nkind: page\nmodule: bills\nroutes: [selfcare.bills]\n---\nTenant text.\n",
            ),
            src(
                "guide/id/tenant/bills/selfcare-bills.md",
                "---\ntitle: Tagihan Saya\nkind: page\nmodule: bills\nroutes: [selfcare.bills]\n---\nTeks.\n",
            ),
        ]
    }

    #[test]
    fn one_guide_per_portal() {
        let d = tempfile::tempdir().unwrap();
        let cfg = portal_config(d.path());
        build(&cfg, &portal_sources(), &State::default()).unwrap();
        let root = read(&cfg, "guide/index.html");
        assert!(
            root.contains(r#"href="en/admin/index.html""#) && root.contains("Admin guide"),
            "{root}"
        );
        assert!(
            root.contains(r#"href="en/tenant/index.html""#) && root.contains(r#"href="id/tenant/index.html""#),
            "{root}"
        );
        let tenant = read(&cfg, "guide/en/tenant/index.html");
        assert!(tenant.contains("My Bills") && !tenant.contains("Orders"), "{tenant}");
        let bills = read(&cfg, "guide/en/tenant/bills/selfcare-bills.html");
        assert!(
            !bills.contains("admin-orders"),
            "a tenant page must not list admin guides"
        );
        assert!(
            bills.contains(r#"href="../../../id/tenant/bills/selfcare-bills.html""#),
            "{bills}"
        );
    }

    #[test]
    fn guide_folder_stands_alone_for_publishing() {
        let d = tempfile::tempdir().unwrap();
        let cfg = portal_config(d.path());
        build(&cfg, &portal_sources(), &State::default()).unwrap();
        let page = read(&cfg, "guide/en/admin/orders/admin-orders.html");
        assert!(page.contains(r#"href="../../../assets/doctest.css""#), "{page}");
        assert!(page.contains(r#"src="../../../assets/search.js""#));
        assert!(
            page.contains(r#"src="../../../img/en/orders/admin-orders-1.png""#),
            "{page}"
        );
        for bad in ["../../../../index.html", "system/", "../../../../assets/"] {
            assert!(!page.contains(bad), "published page links outside guide/: {bad}");
        }
        let gsearch = read(&cfg, "guide/assets/search.js");
        assert!(
            gsearch.contains("My Bills") && !gsearch.contains("Configuration") && !gsearch.contains("Start here"),
            "{gsearch}"
        );
        assert!(
            gsearch.contains("\"url\":\"en/tenant/bills/selfcare-bills.html\""),
            "{gsearch}"
        );
        assert!(cfg.out.join("guide/assets/doctest.css").is_file());
        let internal = read(&cfg, "assets/search.js");
        assert!(!internal.contains("My Bills") && internal.contains("Configuration"));
        assert!(read(&cfg, "index.html").contains(r#"href="guide/index.html""#));
    }

    #[test]
    fn search_text_has_no_table_pipes() {
        assert!(!plain("| A | B |\n| --- | --- |\n| x | y |\n").contains('|'));
    }

    #[test]
    fn escapes_titles() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        build(
            &cfg,
            &[src("system/x.md", "---\ntitle: A <b> & C\n---\nx\n")],
            &State::default(),
        )
        .unwrap();
        assert!(read(&cfg, "system/x.html").contains("A &lt;b&gt; &amp; C"));
    }
}
