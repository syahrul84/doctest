//! Portals: the sign-in areas of an app (admin, selfcare, contractor, a Filament panel, a Blade member area), found
//! from its login pages. Each screen belongs to the portal named in its URL or route name; with one login page every
//! screen belongs to it; screens outside every portal are public (no sign-in). `doctest login` signs in once per
//! portal, and each portal gets its own guide.

use crate::config::Portal;

/// What portal detection needs to know about a screen.
#[derive(Debug, Clone)]
pub struct Screen {
    pub path: String,
    pub name: String,
    /// vue, nuxt, filament, blade
    pub stack: String,
}

/// Path segments that do not name a portal (`/session/admin/login` → admin).
const GENERIC: [&str; 9] = ["session", "sessions", "auth", "account", "accounts", "user", "users", "sign", "web"];

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty() && !s.starts_with(':') && !s.starts_with('{')).collect()
}

fn is_login(path: &str) -> bool {
    matches!(segments(path).last().copied(), Some("login" | "signin" | "sign-in"))
}

fn title_of(name: &str) -> String {
    let mut c = name.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default().replace(['-', '_'], " ") + " guide"
}

pub fn detect(screens: &[Screen]) -> Vec<Portal> {
    let mut out: Vec<Portal> = Vec::new();
    let mut add = |name: &str, login: String| {
        if !out.iter().any(|p| p.name == name) {
            out.push(Portal { name: name.to_string(), title: title_of(name), pages: Vec::new(), user: None, login_url: Some(login) });
        }
    };
    for s in screens {
        if is_login(&s.path) {
            let segs = segments(&s.path);
            let name = segs[..segs.len() - 1].iter().rev().find(|x| !GENERIC.contains(x)).copied().unwrap_or("app");
            add(name, s.path.clone());
        } else if s.stack == "filament"
            && let Some(panel) = segments(&s.path).first()
        {
            // a Filament panel signs in at /<panel>/login
            add(panel, format!("/{panel}/login"));
        }
    }
    if out.len() == 1 && out[0].name == "app" {
        out[0].pages = vec!["*".into()];
        return out;
    }
    for p in out.iter_mut() {
        let n = &p.name;
        p.pages = vec![format!("{n}.*"), format!("/{n}/*"), format!("/{n}"), format!("filament.{n}.*")];
        if let Some(login) = &p.login_url {
            p.pages.push(login.clone());
        }
    }
    out
}

/// `[guide.portals.<name>]` blocks for .doctest.toml.
pub fn to_toml(portals: &[Portal]) -> String {
    let mut t = String::new();
    for p in portals {
        let pages = p.pages.iter().map(|x| format!("\"{x}\"")).collect::<Vec<_>>().join(", ");
        t += &format!("[guide.portals.{}]\ntitle = \"{}\"\npages = [{pages}]\n", p.name, p.title);
        if let Some(l) = &p.login_url {
            t += &format!("login_url = \"{l}\"\n");
        }
        t += "\n";
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(path: &str, name: &str, stack: &str) -> Screen {
        Screen { path: path.into(), name: name.into(), stack: stack.into() }
    }

    #[test]
    fn several_login_pages_make_several_portals() {
        let screens = vec![
            page("/session/admin/login", "session.admin.login", "vue"),
            page("/session/selfcare/login", "session.selfcare.login", "vue"),
            page("/session/contractor/login", "session.contractor.login", "vue"),
            page("/admin/rems/selfcare/users-login", "admin.rems.selfcare.users-login", "vue"),
            page("/admin/rems/data/lease-out", "admin.rems.data.lease-out", "vue"),
        ];
        let p = detect(&screens);
        let names: Vec<&str> = p.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["admin", "selfcare", "contractor"], "users-login is a screen, not a login");
        assert_eq!(p[0].login_url.as_deref(), Some("/session/admin/login"));
        assert_eq!(p[0].title, "Admin guide");
        assert!(crate::work::page_matches("admin.rems.data.lease-out", "/admin/rems/data/lease-out", &p[0].pages));
        assert!(crate::work::page_matches("selfcare.bills", "/selfcare/bills", &p[1].pages));
        assert!(!crate::work::page_matches("selfcare.bills", "/selfcare/bills", &p[0].pages));
    }

    #[test]
    fn one_login_page_covers_every_screen() {
        let p = detect(&[page("/login", "login", "blade"), page("/orders", "orders.index", "vue")]);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].name.as_str(), p[0].login_url.as_deref()), ("app", Some("/login")));
        assert!(crate::work::page_matches("orders.index", "/orders", &p[0].pages));
    }

    #[test]
    fn filament_panel_and_blade_member_area() {
        let p = detect(&[
            page("/admin/ad-orders", "filament.admin.resources.ad-orders.index", "filament"),
            page("/portal/login", "portal.login", "blade"),
            page("/portal/dashboard", "portal.dashboard", "blade"),
            page("/", "home", "blade"),
        ]);
        let names: Vec<&str> = p.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["admin", "portal"]);
        assert_eq!(p[0].login_url.as_deref(), Some("/admin/login"));
        assert!(crate::work::page_matches("filament.admin.resources.ad-orders.index", "/admin/ad-orders", &p[0].pages));
        assert!(crate::work::page_matches("portal.dashboard", "/portal/dashboard", &p[1].pages));
        assert!(!p.iter().any(|x| crate::work::page_matches("home", "/", &x.pages)), "the public home page has no portal");
    }

    #[test]
    fn no_login_means_no_portals() {
        assert!(detect(&[page("/", "home", "blade"), page("/about", "about", "blade")]).is_empty());
    }

    #[test]
    fn toml_for_detected_portals() {
        let p = detect(&[page("/session/admin/login", "session.admin.login", "vue")]);
        let t = to_toml(&p);
        assert!(t.contains("[guide.portals.admin]") && t.contains("login_url = \"/session/admin/login\""), "{t}");
        let parsed: toml::Table = toml::from_str(&t).unwrap();
        assert!(parsed["guide"]["portals"]["admin"]["pages"].as_array().is_some());
    }
}
