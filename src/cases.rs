//! The cases a target should be tested for, in a fixed vocabulary, so the same thing is never tested twice.
//! Targets: an endpoint `METHOD uri`, or a class method `App\Class::method`.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::pages::Endpoint;
use crate::state::Covered;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Case {
    pub target: String,
    pub case: String,
    pub why: String,
}

static VOCAB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(ok|guest|forbidden|not_found|invalid:[^:\s]+:(?:[a-z_]+|\*)|boundary:[^:\s]+:(?:min|max)|filter:[^:\s]+|state:[a-z0-9_]+|throws:[A-Za-z0-9_\\]+|returns:[a-z0-9_]+)$",
    )
    .unwrap()
});

pub fn valid(case: &str) -> bool {
    VOCAB.is_match(case)
}

/// Rules whose violation is worth one negative case each.
const CHECKED_RULES: [&str; 18] = [
    "required",
    "email",
    "numeric",
    "integer",
    "date",
    "in",
    "unique",
    "exists",
    "max",
    "min",
    "size",
    "regex",
    "boolean",
    "array",
    "between",
    "digits",
    "confirmed",
    "after",
];

pub fn endpoint_target(e: &Endpoint) -> String {
    format!("{} {}", e.method, e.uri)
}

/// The action's PHP source (the controller method), read from `e.at` in the backend.
pub fn action_source(backend: &std::path::Path, e: &Endpoint) -> Option<String> {
    let at = e.at.as_deref()?;
    let file = at.rsplit_once(':').map(|(f, _)| f).unwrap_or(at);
    let text = std::fs::read_to_string(backend.join(file)).ok()?;
    let method = e.action.rsplit_once('@').map(|(_, m)| m).unwrap_or("__invoke");
    let (s, end) = crate::testwrite::function_span(&text, method)?;
    Some(text[s..end].to_string())
}

static SIG_PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:\??([A-Za-z_\\][\w\\]*)\s+)?&?\$(\w+)").unwrap());
const SCALARS: [&str; 9] = [
    "string", "int", "integer", "bool", "float", "array", "mixed", "null", "?string",
];

/// Whether a route parameter names a record the action looks up (so a missing one answers 404): a model bound by
/// name (`Order $order`), or a value passed to find/findOrFail/firstOrFail/abort. A flag or label (`{gto}` 0/1,
/// `{floor}`) used only in conditions does not.
pub fn names_record(uri: &str, src: &str) -> bool {
    let params: Vec<String> = uri
        .split('{')
        .skip(1)
        .filter_map(|p| p.split('}').next())
        .map(|p| p.trim_end_matches('?').split(':').next().unwrap_or(p).to_string())
        .collect();
    if params.is_empty() {
        return false;
    }
    let Some(open) = src.find("function") else { return true };
    let after = &src[open..];
    let (Some(lp), Some(rp)) = (after.find('('), after.find(')')) else {
        return true;
    };
    if rp < lp {
        return true;
    }
    let sig = &after[lp + 1..rp];
    let body = &after[rp..];
    let norm = |s: &str| s.to_lowercase().replace('_', "");
    let mut scalar_vars: Vec<String> = Vec::new();
    for c in SIG_PARAM.captures_iter(sig) {
        let ty = c.get(1).map(|m| m.as_str()).unwrap_or("");
        let var = c[2].to_string();
        let class = ty
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase() || ch == '\\')
            && !SCALARS.contains(&ty.to_lowercase().as_str());
        if class {
            // route model binding: a class parameter named like a route parameter
            let short = ty.rsplit('\\').next().unwrap_or(ty);
            if !short.ends_with("Request") && params.iter().any(|p| norm(p) == norm(&var)) {
                return true;
            }
        } else {
            scalar_vars.push(var);
        }
    }
    // a scalar parameter: by name, else by position among the scalar ones
    let mut vars: Vec<String> = Vec::new();
    for (i, p) in params.iter().enumerate() {
        match scalar_vars.iter().find(|v| norm(v) == norm(p)) {
            Some(v) => vars.push(v.clone()),
            None => {
                if let Some(v) = scalar_vars.get(i) {
                    vars.push(v.clone());
                }
            }
        }
    }
    static LOOKUP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:find\w*|first\w*|sole|value|exists|abort\w*|(?:get|fetch|load|show)\w*)\s*\(|ModelNotFound|NotFoundHttpException").unwrap()
    });
    body.split([';', '{', '}']).any(|stmt| {
        vars.iter().any(|v| {
            Regex::new(&format!(r"\${}\b", regex::escape(v)))
                .map(|re| re.is_match(stmt))
                .unwrap_or(false)
                && LOOKUP.is_match(stmt)
        })
    })
}

/// Cases of an endpoint; with the backend, the action's source decides whether a route parameter is a record
/// (not_found) or a plain value.
pub fn for_endpoint(e: &Endpoint, backend: Option<&std::path::Path>) -> Vec<Case> {
    let target = endpoint_target(e);
    let mut out = Vec::new();
    let mut add = |case: String, why: String| {
        if !out.iter().any(|c: &Case| c.case == case) {
            out.push(Case {
                target: target.clone(),
                case,
                why,
            });
        }
    };
    add("ok".into(), "a valid request succeeds".into());
    if let Some(m) = e
        .middleware
        .iter()
        .find(|m| m.as_str() == "auth" || m.starts_with("auth:"))
    {
        add(
            "guest".into(),
            format!("{m} middleware: a request without signing in is refused"),
        );
    }
    if !e.permissions.is_empty() {
        add(
            "forbidden".into(),
            format!(
                "needs permission {}: a signed-in user without it is refused",
                e.permissions.join(" or ")
            ),
        );
    }
    let record = e.uri.contains('{')
        && match backend.and_then(|b| action_source(b, e)) {
            Some(src) => names_record(&e.uri, &src),
            None => true,
        };
    if record {
        add(
            "not_found".into(),
            "the URI names a record: an id that does not exist answers 404".into(),
        );
    }
    for (field, rules) in &e.rules {
        for rule in rules {
            let name = rule.split(':').next().unwrap_or_default().to_lowercase();
            if !CHECKED_RULES.contains(&name.as_str()) {
                continue;
            }
            add(
                format!("invalid:{field}:{name}"),
                format!("rule {rule} on {field}: a value breaking it is refused"),
            );
            let bounds: &[&str] = match name.as_str() {
                "min" => &["min"],
                "max" => &["max"],
                "size" | "between" | "digits" => &["min", "max"],
                _ => &[],
            };
            for b in bounds {
                add(
                    format!("boundary:{field}:{b}"),
                    format!("rule {rule} on {field}: the exact {b} value is accepted"),
                );
            }
        }
    }
    out
}

static THROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"throw\s+new\s+\\?([A-Za-z0-9_\\]+)").unwrap());

/// Cases for a class method from its source: it returns normally, and each exception it throws.
pub fn for_method(target: &str, source: &str) -> Vec<Case> {
    let mut out = vec![Case {
        target: target.into(),
        case: "returns:ok".into(),
        why: "a normal call returns its result".into(),
    }];
    for c in THROW.captures_iter(source) {
        let class = c[1].rsplit('\\').next().unwrap_or_default().to_string();
        let case = format!("throws:{class}");
        if !out.iter().any(|x| x.case == case) {
            out.push(Case {
                target: target.into(),
                case,
                why: format!("throws {class} when its condition holds"),
            });
        }
    }
    out
}

/// The ledger entry covering `case` on `target`; `invalid:<field>:*` covers every rule of that field.
pub fn covered_by<'a>(ledger: &'a [Covered], target: &str, case: &str) -> Option<&'a Covered> {
    ledger.iter().find(|c| {
        c.target == target
            && (c.case == case
                || c.case
                    .strip_suffix('*')
                    .is_some_and(|prefix| prefix.starts_with("invalid:") && case.starts_with(prefix)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::Endpoint;
    use std::collections::BTreeMap;

    fn order() -> Endpoint {
        Endpoint {
            method: "POST".into(),
            uri: "api/orders/{order}".into(),
            middleware: vec![
                "api".into(),
                "auth:sanctum".into(),
                "check.permission:orders/create".into(),
            ],
            permissions: vec!["orders/create".into()],
            rules: BTreeMap::from([
                (
                    "email".to_string(),
                    vec!["required".into(), "email".into(), "max:255".into()],
                ),
                ("note".to_string(), vec!["nullable".into(), "string".into()]),
            ]),
            ..Endpoint::default()
        }
    }

    fn keys(cases: &[Case]) -> Vec<String> {
        cases.iter().map(|c| c.case.clone()).collect()
    }

    #[test]
    fn endpoint_cases() {
        let c = for_endpoint(&order(), None);
        assert_eq!(
            keys(&c),
            vec![
                "ok",
                "guest",
                "forbidden",
                "not_found",
                "invalid:email:required",
                "invalid:email:email",
                "invalid:email:max",
                "boundary:email:max"
            ]
        );
        assert!(
            c.iter()
                .all(|x| x.target == "POST api/orders/{order}" && !x.why.is_empty())
        );
    }

    #[test]
    fn public_endpoint_has_no_guest_case() {
        let e = Endpoint {
            method: "GET".into(),
            uri: "api/ping".into(),
            middleware: vec!["api".into()],
            ..Endpoint::default()
        };
        assert_eq!(keys(&for_endpoint(&e, None)), vec!["ok"]);
    }

    #[test]
    fn not_found_only_for_record_parameters() {
        // route model binding
        assert!(names_record(
            "orders/{order}",
            "public function show(Order $order) { return $order; }"
        ));
        // a plain id looked up
        assert!(names_record(
            "orders/{id}",
            "public function show(Request $r, $id) {\n $o = Order::findOrFail($id);\n return $o; }"
        ));
        // a flag used only in a condition
        assert!(!names_record(
            "import/{gto}/download-template",
            "public function template(Request $request, $gto) {\n if ($gto == 1) { $name = 'gto.xlsx'; } else { $name = 'x.xlsx'; }\n return response()->download($name); }"
        ));
        // a label used as a filter value
        assert!(!names_record(
            "location/{floor}",
            "public function index(string $floor) {\n return Location::where('floor', $floor)->paginate(); }"
        ));
        // a service class injected beside a scalar parameter is not a binding
        assert!(!names_record(
            "report/{year}",
            "public function show(ReportService $svc, int $year) { return $svc->build($year); }"
        ));
    }

    #[test]
    fn method_cases() {
        let src = "public function close(Order $o) {\n if ($o->closed) { throw new \\App\\Exceptions\\AlreadyClosed('x'); }\n throw new InvalidArgumentException();\n}";
        let c = for_method("App\\Services\\Orders::close", src);
        assert_eq!(
            keys(&c),
            vec!["returns:ok", "throws:AlreadyClosed", "throws:InvalidArgumentException"]
        );
    }

    #[test]
    fn vocabulary() {
        for ok in [
            "ok",
            "guest",
            "forbidden",
            "not_found",
            "invalid:email:required",
            "invalid:items.*:integer",
            "boundary:qty:min",
            "filter:status",
            "state:already_approved",
            "throws:AlreadyClosed",
            "returns:ok",
        ] {
            assert!(valid(ok), "{ok}");
        }
        for bad in [
            "happy",
            "boundary:qty:middle",
            "invalid:email",
            "state:Already Approved",
            "",
        ] {
            assert!(!valid(bad), "{bad}");
        }
    }

    #[test]
    fn wildcard_rule_covers_any_rule_of_field() {
        let led = vec![crate::state::Covered {
            target: "T".into(),
            case: "invalid:email:*".into(),
            test: "X::y".into(),
            file: "f".into(),
            source: crate::state::CoverSource::Inferred,
            bug: None,
        }];
        assert!(covered_by(&led, "T", "invalid:email:max").is_some());
        assert!(covered_by(&led, "T", "invalid:qty:max").is_none());
        assert!(covered_by(&led, "U", "invalid:email:max").is_none());
    }
}
