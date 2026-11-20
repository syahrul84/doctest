//! Which cases existing tests already cover: doctest's own rows, laravel-parity's generated cases, and hand-written
//! tests (inferred from the request they send and what they assert).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::config::Config;
use crate::pages::Endpoint;
use crate::state::{CoverSource, Covered};
use crate::testwrite::provider_rows;

pub fn scan(cfg: &Config, endpoints: &[Endpoint]) -> Vec<Covered> {
    let mut files = Vec::new();
    collect(&cfg.backend.join("tests"), &mut files);
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let rel = f.strip_prefix(&cfg.backend).unwrap_or(&f).to_path_buf();
        out.extend(scan_text(cfg, &rel, &text, endpoints));
    }
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "php") {
            out.push(p);
        }
    }
}

static CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:final\s+|abstract\s+)*class\s+(\w+)").unwrap());
static DOCTEST_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)@doctest ([^\n*]+?)\s*\n.*?function\s+(\w+)\s*\(").unwrap());
static PARITY_ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)/\*\* ((?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)[A-Z|]*) (\S+) \*/.*?function\s+(test\w*)\s*\(")
        .unwrap()
});
static PROVIDER_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:@dataProvider\s+|DataProvider\(')(\w+)").unwrap());
static TEST_FN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:[ \t]*(?:#\[[^\n]*Test[^\n]*\]|\*\s*@test)[^\n]*\n)*[ \t]*public function (\w+)\s*\(").unwrap()
});

/// Parts of `text` between `start` and `end` markers, and the text with those parts removed.
fn split_blocks<'a>(text: &'a str, start: &str, end: &str) -> (Vec<&'a str>, String) {
    let mut blocks = Vec::new();
    let mut rest = String::new();
    let mut from = 0;
    while let Some(s) = text[from..].find(start).map(|i| i + from) {
        let e = text[s..].find(end).map(|i| i + s + end.len()).unwrap_or(text.len());
        rest += &text[from..s];
        blocks.push(&text[s..e]);
        from = e;
    }
    rest += &text[from..];
    (blocks, rest)
}

pub fn scan_text(_cfg: &Config, rel: &Path, text: &str, endpoints: &[Endpoint]) -> Vec<Covered> {
    let class = CLASS.captures(text).map(|c| c[1].to_string()).unwrap_or_default();
    let mut out = Vec::new();
    let mut push = |target: &str, case: String, method: &str, source: CoverSource, bug: Option<String>| {
        let test = format!("{class}::{method}");
        if !out
            .iter()
            .any(|c: &Covered| c.target == target && c.case == case && c.test == test)
        {
            out.push(Covered {
                target: target.to_string(),
                case,
                test,
                file: rel.to_path_buf(),
                source,
                bug,
            });
        }
    };

    let (doctest_blocks, rest) = split_blocks(text, crate::testwrite::START, crate::testwrite::END);
    for block in doctest_blocks {
        for c in DOCTEST_TAG.captures_iter(block) {
            let (target, m) = (c[1].trim(), &c[2]);
            for (case, bug) in provider_rows(block, &format!("cases_{m}"))
                .into_iter()
                .chain(provider_rows(block, &format!("bugs_{m}")))
            {
                push(target, case, m, CoverSource::Doctest, bug);
            }
        }
    }

    let (mut parity, rest) = split_blocks(&rest, "// <laravel-parity>", "// </laravel-parity>");
    let (ai, rest) = split_blocks(&rest, "// <laravel-parity-ai>", "// </laravel-parity-ai>");
    parity.extend(ai);
    for block in parity {
        for c in PARITY_ROUTE.captures_iter(block) {
            let method = c[1].split('|').next().unwrap_or_default();
            let target = format!("{method} {}", &c[2]);
            let test = c[3].to_string();
            let Some((s, e)) = crate::testwrite::function_span(block, &test) else {
                continue;
            };
            let Some(provider) = PROVIDER_NAME.captures(&block[s..e]).map(|p| p[1].to_string()) else {
                continue;
            };
            for (key, row) in parity_rows(block, &provider) {
                match key.as_str() {
                    "happy" | "ok" => push(&target, "ok".into(), &test, CoverSource::Parity, None),
                    "guest" => push(&target, "guest".into(), &test, CoverSource::Parity, None),
                    "missing" => push(&target, "not_found".into(), &test, CoverSource::Parity, None),
                    "forbidden" => push(&target, "forbidden".into(), &test, CoverSource::Parity, None),
                    _ => {
                        for field in last_list(&row) {
                            push(
                                &target,
                                format!("invalid:{field}:required"),
                                &test,
                                CoverSource::Parity,
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    let spans = if TEST_FN.is_match(&rest) {
        crate::testwrite::all_function_spans(&rest)
    } else {
        Default::default()
    };
    for c in TEST_FN.captures_iter(&rest) {
        let m = c[1].to_string();
        if !m.starts_with("test") && !c[0].contains("Test") && !c[0].contains("@test") {
            continue;
        }
        let Some(&(s, e)) = spans.get(&m) else { continue };
        let body = &rest[s..e];
        let hits = requests(body, endpoints);
        if hits.is_empty() {
            continue;
        }
        let cases = asserted_cases(body);
        for target in &hits {
            for case in &cases {
                push(target, case.clone(), &m, CoverSource::Inferred, None);
            }
        }
    }
    out
}

/// (row key, row text) of a laravel-parity provider.
fn parity_rows(block: &str, provider: &str) -> Vec<(String, String)> {
    let Some((s, e)) = crate::testwrite::function_span(block, provider) else {
        return Vec::new();
    };
    let row = Regex::new(r"(?m)^\s*'([^']+)'\s*=>\s*(\[.*\]),?\s*$").unwrap();
    row.captures_iter(&block[s..e])
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// Quoted strings in the last `[...]` of a row (laravel-parity's validation error fields).
fn last_list(row: &str) -> Vec<String> {
    let inner = Regex::new(r"\[([^\[\]]*)\]").unwrap();
    let Some(last) = inner.captures_iter(row).last() else {
        return Vec::new();
    };
    let q = Regex::new(r"'([^']+)'").unwrap();
    q.captures_iter(&last[1]).map(|c| c[1].to_string()).collect()
}

static REQUEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"->(getJson|postJson|putJson|patchJson|deleteJson|get|post|put|patch|delete|json)\(\s*(?:'(GET|POST|PUT|PATCH|DELETE)'\s*,\s*)?((?:route\(\s*'[^']+')|'[^']*'|"[^"]*")(\s*\.\s*)?"#).unwrap()
});

/// `report-{year}` → `^report-[^/]+$`.
fn segment_regex(seg: &str) -> Regex {
    let mut s = String::from("^");
    for (i, part) in seg.split(['{', '}']).enumerate() {
        if i % 2 == 1 {
            s += "[^/]+";
        } else {
            s += &regex::escape(part);
        }
    }
    Regex::new(&(s + "$")).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

/// A route URI (`api/orders/{order}`, `{x?}` optional) matches a URI a test sends (`/api/orders/X`).
fn uri_matches(route: &str, uri: &str) -> bool {
    let r: Vec<&str> = route.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let u: Vec<&str> = uri.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if u.len() > r.len() {
        return false;
    }
    for (i, seg) in r.iter().enumerate() {
        let param = seg.starts_with('{') && seg.ends_with('}') && seg.matches('{').count() == 1;
        let partial = !param && seg.contains('{');
        match u.get(i) {
            Some(x) if param || x == seg => {}
            Some(x) if partial && segment_regex(seg).is_match(x) => {}
            None if param && seg.ends_with("?}") => {}
            _ => return false,
        }
    }
    true
}

/// Endpoint targets a test body sends requests to.
fn requests(body: &str, endpoints: &[Endpoint]) -> Vec<String> {
    let mut out = Vec::new();
    for c in REQUEST.captures_iter(body) {
        let helper = &c[1];
        let method = match c.get(2) {
            Some(m) => m.as_str().to_string(),
            None => helper.trim_end_matches("Json").to_uppercase(),
        };
        let arg = &c[3];
        let found = if let Some(name) = arg.strip_prefix("route(") {
            let name = name.trim().trim_matches('\'').to_string();
            endpoints
                .iter()
                .find(|e| e.method == method && e.route_name.as_deref() == Some(name.as_str()))
        } else {
            let mut uri = arg
                .trim_matches(['\'', '"'])
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            uri = PHP_VAR.replace_all(&uri, "X").into_owned();
            if c.get(4).is_some() {
                uri += "X";
            }
            endpoints
                .iter()
                .find(|e| e.method == method && uri_matches(&e.uri, &uri))
        };
        if let Some(e) = found {
            let t = crate::cases::endpoint_target(e);
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

static PHP_VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\$[^}]+\}|\$\w+(?:->\w+)*").unwrap());
static STATUS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"assertStatus\(\s*(\d{3})\s*\)").unwrap());
static VALIDATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:assertJsonValidationErrors|assertInvalid|assertJsonValidationErrorFor)\(\s*(\[[^\]]*\]|'[^']*')")
        .unwrap()
});

/// Cases a test body asserts.
fn asserted_cases(body: &str) -> Vec<String> {
    let mut codes: Vec<u16> = STATUS.captures_iter(body).filter_map(|c| c[1].parse().ok()).collect();
    for (name, code) in [
        ("assertOk(", 200),
        ("assertCreated(", 201),
        ("assertNoContent(", 204),
        ("assertSuccessful(", 200),
        ("assertUnauthorized(", 401),
        ("assertForbidden(", 403),
        ("assertNotFound(", 404),
        ("assertUnprocessable(", 422),
    ] {
        if body.contains(name) {
            codes.push(code);
        }
    }
    let mut out = Vec::new();
    for code in codes {
        let case = match code {
            200..=299 => "ok",
            401 => "guest",
            403 => "forbidden",
            404 => "not_found",
            _ => continue,
        };
        if !out.iter().any(|c| c == case) {
            out.push(case.to_string());
        }
    }
    let q = Regex::new(r"'([^']+)'(\s*=>)?").unwrap();
    for c in VALIDATION.captures_iter(body) {
        for f in q.captures_iter(&c[1]) {
            if c[1].starts_with('[') && c[1].contains("=>") && f.get(2).is_none() {
                continue; // a message value, not a field
            }
            let field = f[1].to_string();
            let sent = body.contains(&format!("'{field}' =>")) || body.contains(&format!("\"{field}\" =>"));
            let case = if sent {
                format!("invalid:{field}:*")
            } else {
                format!("invalid:{field}:required")
            };
            if !out.contains(&case) {
                out.push(case);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::Endpoint;
    use crate::pages::tests::shop_config;

    fn endpoints() -> Vec<Endpoint> {
        vec![Endpoint {
            method: "POST".into(),
            uri: "api/orders/{order}".into(),
            route_name: Some("orders.store".into()),
            ..Endpoint::default()
        }]
    }

    fn has(l: &[Covered], case: &str, source: CoverSource, test: &str) -> bool {
        l.iter().any(|c| {
            c.target == "POST api/orders/{order}" && c.case == case && c.source == source && c.test.ends_with(test)
        })
    }

    #[test]
    fn ledger_reads_hand_written_and_parity() {
        let d = tempfile::tempdir().unwrap();
        let l = scan(&shop_config(d.path()), &endpoints());
        assert!(
            has(
                &l,
                "invalid:email:required",
                CoverSource::Inferred,
                "test_order_needs_an_email"
            ),
            "{l:#?}"
        );
        assert!(
            has(&l, "ok", CoverSource::Inferred, "test_order_saves_through_named_route"),
            "{l:#?}"
        );
        assert!(
            has(&l, "guest", CoverSource::Parity, "test_post_api_orders_order"),
            "{l:#?}"
        );
        assert!(
            has(
                &l,
                "invalid:qty:required",
                CoverSource::Parity,
                "test_post_api_orders_order"
            ),
            "{l:#?}"
        );
        assert!(!l.iter().any(|c| c.test.ends_with("test_unrelated")));
        assert!(
            l.iter()
                .all(|c| c.file == *"tests/Feature/Http/Controllers/OrderControllerTest.php")
        );
    }

    #[test]
    fn route_uri_matching() {
        assert!(uri_matches("api/orders/{order}", "/api/orders/X"));
        assert!(uri_matches("api/orders/{order?}", "/api/orders"));
        assert!(!uri_matches("api/orders/{order}", "/api/orders"));
        assert!(!uri_matches("api/orders", "/api/orders/5"));
        assert!(!uri_matches("api/orders/{order}", "/api/bills/5"));
        assert!(uri_matches("api/report-{year}", "/api/report-2026"));
        assert!(uri_matches("api/files/{file}.{ext}", "/api/files/a.pdf"));
    }

    #[test]
    fn ledger_reads_doctest_tags() {
        let d = tempfile::tempdir().unwrap();
        let cfg = shop_config(d.path());
        let f = d.path().join("ATest.php");
        let ok = crate::testwrite::Row {
            case: "ok".into(),
            args_php: "200".into(),
            bug: None,
        };
        let bug = crate::testwrite::Row {
            case: "forbidden".into(),
            args_php: "403".into(),
            bug: Some("returns 200 without the permission".into()),
        };
        let t = crate::testwrite::NewTests {
            method: "test_store".into(),
            method_php: Some("public function test_store(int $s): void {}".into()),
            rows: vec![ok, bug],
        };
        crate::testwrite::insert(&f, "ATest", "Tests", "POST api/orders/{order}", &t).unwrap();
        let php = std::fs::read_to_string(&f).unwrap();
        let l = scan_text(&cfg, Path::new("tests/ATest.php"), &php, &endpoints());
        assert_eq!(l.len(), 2, "{l:#?}");
        assert!(l.iter().all(|c| c.source == CoverSource::Doctest
            && c.test == "ATest::test_store"
            && c.target == "POST api/orders/{order}"));
        assert_eq!(l[0].case, "ok");
        assert_eq!(l[0].bug, None);
        assert_eq!(l[1].case, "forbidden");
        assert_eq!(l[1].bug.as_deref(), Some("returns 200 without the permission"));
    }
}
