//! Which web origin a hosted site is served on, and the headers that follow
//! from it.
//!
//! A site is reached three ways:
//!
//! * under the shared path `<gateway>/s/<site_id>/` ([`Served::Shared`]).
//!   Every site there, and the gateway's own APIs, are one origin to a
//!   browser, so pages are sandboxed: each document gets an origin of its
//!   own that no other document shares;
//! * on a per-site host `<site_id>.<suffix>` ([`Served::SiteHost`]), when
//!   the operator set `[gateway] site_host_suffix`. The host is the site's
//!   origin, and nothing else is served on it;
//! * on a custom domain the owner signed into the record and DNS confirms
//!   ([`Served::Domain`]). The domain is the site's origin.
//!
//! On the last two a page is an ordinary page: localStorage, cookies and
//! service workers are its own.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// How a response reaches the visitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// `<gateway>/s/<site_id>/...`
    Shared,
    /// `<site_id>.<suffix>/...`
    SiteHost,
    /// A custom domain of the site.
    Domain,
}

/// The policy on pages under the shared path. `sandbox` without
/// `allow-same-origin` gives the document an opaque origin: it runs its
/// scripts and forms, but has no localStorage, cookies or service worker,
/// and its requests to this gateway are cross-origin ones.
pub const SHARED_SANDBOX: &str = "sandbox allow-scripts allow-forms allow-popups allow-modals allow-downloads \
allow-pointer-lock allow-presentation";

/// Env overrides of `[gateway] site_host_suffix` / `sandbox_shared_sites`.
pub const SITE_HOST_SUFFIX_ENV: &str = "C0MPUTE_SITE_HOST_SUFFIX";
pub const SANDBOX_SHARED_SITES_ENV: &str = "C0MPUTE_SANDBOX_SHARED_SITES";

/// Whether a response of this type is a document a browser runs scripts
/// from when it is opened: HTML, and the XML types (an SVG opened on its own
/// is a document too).
pub fn active_document(content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(
        ct.as_str(),
        "text/html" | "application/xhtml+xml" | "image/svg+xml" | "text/xml" | "application/xml"
    ) || ct.ends_with("+xml")
}

/// Add the headers a hosted response needs for where it is served.
/// `sandbox`: `[gateway] sandbox_shared_sites`.
///
/// Everywhere: `X-Content-Type-Options: nosniff`, when absent. Under the
/// shared path, documents also get `Referrer-Policy: no-referrer` (the URL
/// names the gateway and the site) and, with `sandbox`, the sandbox policy;
/// and every response then allows cross-origin reads, since a sandboxed
/// page is cross-origin to its own assets and could not otherwise `fetch`
/// them, load its fonts or import its modules. Everything under `/s/` is
/// public and served without credentials, so that gives nothing away.
pub fn apply(headers: &mut HeaderMap, served: Served, sandbox: bool) {
    let absent = |headers: &mut HeaderMap, name: &'static str, value: &'static str| {
        if !headers.contains_key(name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    };
    absent(headers, "x-content-type-options", "nosniff");
    if served != Served::Shared {
        return;
    }
    let document = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(active_document);
    if document {
        absent(headers, "referrer-policy", "no-referrer");
    }
    if !sandbox {
        return;
    }
    if document {
        // Appended, not replaced: a policy already on the response stays
        // in force next to this one.
        headers.append(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(SHARED_SANDBOX));
    }
    absent(headers, "access-control-allow-origin", "*");
    absent(headers, "cross-origin-resource-policy", "cross-origin");
}

/// Whether a request is a browser opening a page (a navigation, in a tab or
/// a frame) rather than a fetch of a file: `Sec-Fetch-Mode: navigate`, or
/// for a browser too old to send that, an `Accept` that asks for HTML.
/// Scripts, feed readers, podcast apps, `curl` and the `/p/` loader's fetch
/// of its ciphertext are not navigations.
pub fn wants_document(headers: &HeaderMap) -> bool {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_ascii_lowercase());
    match get("sec-fetch-mode") {
        Some(mode) => mode == "navigate",
        None => get("accept").is_some_and(|a| a.contains("text/html")),
    }
}

/// Send a page opened under `/s/<site_id>/` to the site's own host.
///
/// 307: the method is kept, and unlike a 301 or 308 a browser does not
/// remember it for good, so an operator who takes the suffix away again is
/// not left with visitors stuck on a dead name. Caches may keep it for five
/// minutes, per kind of request (only navigations are redirected).
pub fn redirect_to_site_host(url: &str) -> Response {
    let Ok(location) = HeaderValue::from_str(url) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mut resp = StatusCode::TEMPORARY_REDIRECT.into_response();
    let h = resp.headers_mut();
    h.insert(header::LOCATION, location);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    vary_on_navigation(h);
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    resp
}

/// With per-site hosts on, what `/s/...` answers depends on whether the
/// request is a navigation: say so to caches.
pub fn vary_on_navigation(headers: &mut HeaderMap) {
    headers.append(header::VARY, HeaderValue::from_static("Sec-Fetch-Mode, Accept"));
}

/// `(site_host_suffix, sandbox_shared_sites)` as this machine is configured:
/// the env overrides, else the `[gateway]` section of the config file in its
/// usual place, else the defaults (no suffix, sandbox on).
pub fn configured() -> (Option<String>, bool) {
    let file = crate::config::Config::load_raw(&crate::config::default_config_path())
        .map(|c| c.gateway)
        .unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    resolve(
        file.site_host_suffix,
        file.sandbox_shared_sites,
        env(SITE_HOST_SUFFIX_ENV),
        env(SANDBOX_SHARED_SITES_ENV),
    )
}

/// [`configured`] from its inputs. A suffix that is not a DNS name is
/// dropped (sites stay under `/s/`) rather than half-applied.
pub fn resolve(
    file_suffix: Option<String>,
    file_sandbox: bool,
    env_suffix: Option<String>,
    env_sandbox: Option<String>,
) -> (Option<String>, bool) {
    let suffix = env_suffix.or(file_suffix).filter(|s| !s.trim().is_empty()).and_then(|raw| {
        let ok = c0mpute_gateway::site_host::normalize_suffix(&raw);
        if ok.is_none() {
            tracing::warn!(suffix = %raw, "hosting: site_host_suffix is not a domain name, ignored");
        }
        ok
    });
    (suffix, crate::config::auto_install_enabled_with(file_sandbox, env_sandbox))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_for(content_type: Option<&str>, served: Served, sandbox: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(ct) = content_type {
            h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
        }
        apply(&mut h, served, sandbox);
        h
    }

    fn get<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
        h.get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn pages_under_the_shared_path_are_sandboxed_and_their_assets_readable() {
        let h = headers_for(Some("text/html; charset=utf-8"), Served::Shared, true);
        assert_eq!(get(&h, "content-security-policy"), Some(SHARED_SANDBOX));
        assert_eq!(get(&h, "referrer-policy"), Some("no-referrer"));
        assert_eq!(get(&h, "access-control-allow-origin"), Some("*"));
        assert_eq!(get(&h, "cross-origin-resource-policy"), Some("cross-origin"));
        assert_eq!(get(&h, "x-content-type-options"), Some("nosniff"));
        // The policy gives the page an opaque origin: never allow-same-origin.
        assert!(SHARED_SANDBOX.starts_with("sandbox ") && !SHARED_SANDBOX.contains("allow-same-origin"));
        assert!(!SHARED_SANDBOX.contains("allow-top-navigation"));
        // An asset: readable by the sandboxed page, and not itself sandboxed
        // (the policy would break a PDF opened in the browser's viewer).
        for ct in ["text/css", "application/javascript", "image/png", "application/pdf", "audio/mpeg"] {
            let h = headers_for(Some(ct), Served::Shared, true);
            assert_eq!(get(&h, "content-security-policy"), None, "{ct}");
            assert_eq!(get(&h, "referrer-policy"), None, "{ct}");
            assert_eq!(get(&h, "access-control-allow-origin"), Some("*"), "{ct}");
            assert_eq!(get(&h, "cross-origin-resource-policy"), Some("cross-origin"), "{ct}");
        }
        // A response with no type (a 304, a plain 404) still allows the read.
        let h = headers_for(None, Served::Shared, true);
        assert_eq!(get(&h, "content-security-policy"), None);
        assert_eq!(get(&h, "access-control-allow-origin"), Some("*"));
        assert_eq!(get(&h, "x-content-type-options"), Some("nosniff"));
    }

    #[test]
    fn an_svg_or_xml_opened_as_a_page_is_a_document_too() {
        for ct in ["image/svg+xml", "application/xhtml+xml", "text/xml", "application/xml", "TEXT/HTML", "application/rss+xml"] {
            assert!(active_document(ct), "{ct}");
            let h = headers_for(Some(ct), Served::Shared, true);
            assert_eq!(get(&h, "content-security-policy"), Some(SHARED_SANDBOX), "{ct}");
        }
        for ct in ["text/plain", "application/json", "text/htmlx", "video/mp4", ""] {
            assert!(!active_document(ct), "{ct}");
        }
    }

    #[test]
    fn the_operator_can_turn_the_sandbox_off() {
        let h = headers_for(Some("text/html"), Served::Shared, false);
        assert_eq!(get(&h, "content-security-policy"), None);
        assert_eq!(get(&h, "access-control-allow-origin"), None);
        assert_eq!(get(&h, "cross-origin-resource-policy"), None);
        // The baseline stays.
        assert_eq!(get(&h, "referrer-policy"), Some("no-referrer"));
        assert_eq!(get(&h, "x-content-type-options"), Some("nosniff"));
    }

    #[test]
    fn a_site_on_its_own_origin_is_an_ordinary_page() {
        for served in [Served::SiteHost, Served::Domain] {
            for sandbox in [true, false] {
                let h = headers_for(Some("text/html; charset=utf-8"), served, sandbox);
                assert_eq!(get(&h, "content-security-policy"), None, "{served:?}");
                assert_eq!(get(&h, "access-control-allow-origin"), None, "{served:?}");
                assert_eq!(get(&h, "cross-origin-resource-policy"), None, "{served:?}");
                assert_eq!(get(&h, "referrer-policy"), None, "{served:?}");
                assert_eq!(get(&h, "x-content-type-options"), Some("nosniff"), "{served:?}");
            }
        }
    }

    #[test]
    fn headers_already_set_are_kept_and_a_policy_is_added_to_not_replaced() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
        h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
        h.insert("access-control-allow-origin", HeaderValue::from_static("https://ads.example"));
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'"));
        apply(&mut h, Served::Shared, true);
        assert_eq!(get(&h, "referrer-policy"), Some("same-origin"));
        assert_eq!(get(&h, "access-control-allow-origin"), Some("https://ads.example"));
        let policies: Vec<&str> = h
            .get_all(header::CONTENT_SECURITY_POLICY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(policies, vec!["default-src 'self'", SHARED_SANDBOX]);
        assert_eq!(h.get_all("x-content-type-options").iter().count(), 1);
    }

    #[test]
    fn only_a_browser_opening_a_page_is_a_navigation() {
        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_static(*v));
            }
            h
        };
        assert!(wants_document(&with(&[("sec-fetch-mode", "navigate"), ("accept", "text/html,*/*")])));
        assert!(wants_document(&with(&[("sec-fetch-mode", "Navigate")])));
        // fetch() from a page (the /p/ loader reading its ciphertext), an
        // <img>, a script: served where they asked.
        assert!(!wants_document(&with(&[("sec-fetch-mode", "cors"), ("accept", "text/html")])));
        assert!(!wants_document(&with(&[("sec-fetch-mode", "same-origin")])));
        assert!(!wants_document(&with(&[("sec-fetch-mode", "no-cors"), ("accept", "image/avif,*/*")])));
        // An old browser sends no Sec-Fetch-Mode but asks for HTML.
        assert!(wants_document(&with(&[("accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8")])));
        // curl, reqwest, a feed reader.
        assert!(!wants_document(&with(&[("accept", "*/*")])));
        assert!(!wants_document(&with(&[("accept", "application/rss+xml")])));
        assert!(!wants_document(&HeaderMap::new()));
    }

    #[test]
    fn the_redirect_keeps_the_method_and_is_cached_briefly() {
        let r = redirect_to_site_host("https://0123456789abcdef0123.sites.example.com/a%20b?x=1");
        assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            get(r.headers(), "location"),
            Some("https://0123456789abcdef0123.sites.example.com/a%20b?x=1")
        );
        assert_eq!(get(r.headers(), "cache-control"), Some("public, max-age=300"));
        assert_eq!(get(r.headers(), "vary"), Some("Sec-Fetch-Mode, Accept"));
        assert_eq!(redirect_to_site_host("bad\r\nset-cookie: x").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_env_overrides_the_file_and_a_bad_suffix_is_dropped() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(resolve(None, true, None, None), (None, true), "the default: /s/ only, sandboxed");
        assert_eq!(resolve(s("*.Sites.Example.com"), true, None, None), (s("sites.example.com"), true));
        assert_eq!(resolve(s("a.example.com"), true, s("b.example.com"), None).0, s("b.example.com"));
        assert_eq!(resolve(s("not a domain"), true, None, None).0, None);
        assert_eq!(resolve(s("localhost"), true, None, None).0, None);
        assert_eq!(resolve(s(" "), true, None, None).0, None);
        assert!(!resolve(None, false, None, None).1);
        assert!(!resolve(None, true, None, s("0")).1);
        assert!(!resolve(None, true, None, s("off")).1);
        assert!(resolve(None, false, None, s("1")).1);
    }
}
