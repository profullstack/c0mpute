//! Per-site hostnames: `<site_id>.<suffix>` is one hosted site and nothing
//! else.
//!
//! Every hosted site used to be reachable only at `<gateway>/s/<site_id>/`,
//! one web origin shared by all sites and by the gateway's own APIs (storage
//! writes, git, swarm routes): any site's script could read another site's
//! localStorage and call those APIs as the same origin. With a suffix
//! configured (`[gateway] site_host_suffix`), each site gets an origin of its
//! own, and on that origin the only thing served is the site.
//!
//! This module is the part every router on the gateway shares: telling a
//! per-site host from any other ([`site_of_host`]), and making sure a request
//! on one can reach nothing but site serving ([`isolate`]). The hosting
//! service serves the site itself.

use std::sync::RwLock;

use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};

/// Marks a request that arrived on a per-site host. [`isolate`] adds it
/// after rewriting the path to `/s/<site>/<path>`; whatever serves sites
/// reads it to know the visitor's URL has no `/s/<site>` prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteHost {
    pub site: String,
}

/// The suffix this process serves per-site hosts under, set once the hosting
/// service knows its configuration.
static SUFFIX: RwLock<Option<String>> = RwLock::new(None);

/// Set the per-site host suffix for this process (see [`isolate`]).
pub fn configure(suffix: &str) {
    if let Some(s) = normalize_suffix(suffix) {
        *SUFFIX.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }
}

/// The suffix set with [`configure`], if any.
pub fn configured() -> Option<String> {
    SUFFIX.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A configured suffix in the form hosts are compared with: lower case, no
/// leading `*.` or dots, no trailing dot. `None` when what is left is not a
/// DNS name of at least two labels.
pub fn normalize_suffix(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    let s = s.strip_prefix("*.").unwrap_or(&s).trim_matches('.').to_string();
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    // 20 characters of site id and a dot go in front of it.
    (s.len() <= 232 && s.contains('.') && s.split('.').all(label_ok)).then_some(s)
}

/// A site id: 20 lower-case hex characters (it is one DNS label as it is).
pub fn valid_site_id(s: &str) -> bool {
    s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The site a `Host` value names under `suffix` (already normalized):
/// exactly `<site_id>.<suffix>`, with an optional port and trailing dot.
/// Host names are case-insensitive, so the id is matched in lower case; a
/// name with more labels in front, or anything that is not a site id, is
/// not a per-site host.
pub fn site_of_host(host: &str, suffix: &str) -> Option<String> {
    let host = host.trim();
    let host = match host.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let label = host.strip_suffix(suffix)?.strip_suffix('.')?;
    valid_site_id(label).then(|| label.to_string())
}

/// The host a request is for: its `Host` header, else the authority of its
/// URI (HTTP/2 requests may carry only that).
pub fn request_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.as_str().to_string()))
}

/// Where `/s/<site>/<rest>` lives on the site's own host. `rest` is the path
/// after `/s/<site>/` exactly as the visitor sent it (still percent-encoded);
/// the query is kept. `None` for a site id or suffix that could not be part
/// of a URL.
pub fn site_url(site: &str, suffix: &str, rest: &str, query: Option<&str>) -> Option<String> {
    if !valid_site_id(site) || normalize_suffix(suffix).as_deref() != Some(suffix) {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    Some(match query {
        Some(q) if !q.is_empty() => format!("https://{site}.{suffix}/{rest}?{q}"),
        _ => format!("https://{site}.{suffix}/{rest}"),
    })
}

/// Wrap the whole gateway so that a request on a per-site host can only
/// reach site serving: its path is rewritten to `/s/<site><path>` before any
/// routing (and marked with [`SiteHost`]), so no other route (storage API,
/// git, swarm, the `/p/` loader) can match it, and anything but GET or HEAD
/// is answered 404 here. Requests on every other host pass through
/// untouched. The suffix is read per request from [`configured`].
pub fn isolate(app: Router) -> Router {
    wrap(app, None)
}

/// [`isolate`] with the suffix given rather than read from [`configured`].
pub fn isolate_with(app: Router, suffix: &str) -> Router {
    wrap(app, normalize_suffix(suffix))
}

fn wrap(app: Router, fixed: Option<String>) -> Router {
    // A router with nothing but a fallback: its middleware runs before the
    // wrapped router routes, so the rewritten path is what gets routed.
    Router::new()
        .fallback_service(app)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let suffix = fixed.clone().or_else(configured);
            async move { guard(suffix, req, next).await }
        }))
}

async fn guard(suffix: Option<String>, mut req: Request, next: Next) -> Response {
    let Some(suffix) = suffix else {
        return next.run(req).await;
    };
    let site = request_host(req.headers(), req.uri()).and_then(|h| site_of_host(&h, &suffix));
    // Already rewritten for this host by a wrap further out: leave it.
    if site.is_some() && req.extensions().get::<SiteHost>().map(|m| &m.site) == site.as_ref() {
        return next.run(req).await;
    }
    // Any other mark was not put there for this request's host.
    req.extensions_mut().remove::<SiteHost>();
    let Some(site) = site else {
        return next.run(req).await;
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return not_found();
    }
    let Some(uri) = rewritten(req.uri(), &site) else {
        return not_found();
    };
    *req.uri_mut() = uri;
    req.extensions_mut().insert(SiteHost { site });
    next.run(req).await
}

/// `uri` with its path moved under `/s/<site>` (query kept).
fn rewritten(uri: &Uri, site: &str) -> Option<Uri> {
    let path = uri.path();
    let path = if path.starts_with('/') { path } else { "/" };
    let pq = match uri.query() {
        Some(q) => format!("/s/{site}{path}?{q}"),
        None => format!("/s/{site}{path}"),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(pq.parse().ok()?);
    Uri::from_parts(parts).ok()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    const SITE: &str = "0123456789abcdef0123";
    const SUFFIX: &str = "sites.example.com";

    #[test]
    fn a_site_host_is_exactly_a_site_id_under_the_suffix() {
        let site = |h: &str| site_of_host(h, SUFFIX);
        assert_eq!(site(&format!("{SITE}.{SUFFIX}")).as_deref(), Some(SITE));
        assert_eq!(site(&format!("{SITE}.{SUFFIX}:8443")).as_deref(), Some(SITE));
        assert_eq!(site(&format!("{SITE}.{SUFFIX}.")).as_deref(), Some(SITE));
        // Host names are case-insensitive.
        assert_eq!(site(&format!("{}.SITES.Example.com", SITE.to_uppercase())).as_deref(), Some(SITE));
        // Not a site id: too short, too long, not hex, empty.
        assert_eq!(site(&format!("0123456789abcdef012.{SUFFIX}")), None);
        assert_eq!(site(&format!("0123456789abcdef01234.{SUFFIX}")), None);
        assert_eq!(site(&format!("0123456789abcdefghij.{SUFFIX}")), None);
        assert_eq!(site(&format!(".{SUFFIX}")), None);
        assert_eq!(site(SUFFIX), None);
        // More labels in front, another suffix, or the suffix as a mere
        // ending of another name.
        assert_eq!(site(&format!("evil.{SITE}.{SUFFIX}")), None);
        assert_eq!(site(&format!("{SITE}.example.com")), None);
        assert_eq!(site(&format!("{SITE}.notsites.example.com")), None);
        assert_eq!(site(&format!("{SITE}{SUFFIX}")), None);
        assert_eq!(site(&format!("{SITE}.{SUFFIX}.evil.com")), None);
        assert_eq!(site(&format!("{SITE}.{SUFFIX}:80:80")), None);
        assert_eq!(site(&format!("{SITE}.{SUFFIX}/x")), None);
        assert_eq!(site("gateway.example.com"), None);
        assert_eq!(site(""), None);
    }

    #[test]
    fn suffixes_are_normalized_or_refused() {
        assert_eq!(normalize_suffix(" *.Sites.Example.com. ").as_deref(), Some(SUFFIX));
        assert_eq!(normalize_suffix(".sites.example.com").as_deref(), Some(SUFFIX));
        assert_eq!(normalize_suffix("localhost"), None, "one label: every host would be under it");
        assert_eq!(normalize_suffix(""), None);
        assert_eq!(normalize_suffix("sites.example.com/x"), None);
        assert_eq!(normalize_suffix("sites..example.com"), None);
        assert_eq!(normalize_suffix("-sites.example.com"), None);
        assert_eq!(normalize_suffix(&format!("{}.com", "a".repeat(64))), None);
    }

    #[test]
    fn the_redirect_target_keeps_path_and_query() {
        let url = |rest: &str, q: Option<&str>| site_url(SITE, SUFFIX, rest, q);
        assert_eq!(url("", None).unwrap(), format!("https://{SITE}.{SUFFIX}/"));
        assert_eq!(url("docs/", None).unwrap(), format!("https://{SITE}.{SUFFIX}/docs/"));
        assert_eq!(
            url("a%20b/c.html", Some("x=1&y=%2F")).unwrap(),
            format!("https://{SITE}.{SUFFIX}/a%20b/c.html?x=1&y=%2F"),
            "the path and query go over as they were sent"
        );
        assert_eq!(url("p", Some("")).unwrap(), format!("https://{SITE}.{SUFFIX}/p"));
        // A leading slash cannot turn the path into another host.
        assert_eq!(url("/evil.com/x", None).unwrap(), format!("https://{SITE}.{SUFFIX}/evil.com/x"));
        // Only a real site id and a normalized suffix make a URL.
        assert_eq!(site_url("evil.com/", SUFFIX, "", None), None);
        assert_eq!(site_url(SITE, "Sites.Example.com", "", None), None);
        assert_eq!(site_url(SITE, "example.com/@", "", None), None);
    }

    /// A gateway with an API route, a write route and site serving that
    /// answers with the path it was asked for.
    fn gateway() -> Router {
        Router::new()
            .route("/storage/v1/objects", get(|| async { "api read" }).post(|| async { "api write" }))
            .route("/swarm/v1/run", post(|| async { "swarm" }))
            .route(
                "/s/{site}/{*path}",
                get(|req: Request| async move {
                    let marked = req.extensions().get::<SiteHost>().map(|s| s.site.clone()).unwrap_or_default();
                    format!("site {} marked={marked}", req.uri())
                }),
            )
    }

    async fn ask(app: Router, method: &str, host: &str, uri: &str) -> (StatusCode, String) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn api_routes_are_out_of_reach_on_a_site_host() {
        let app = isolate_with(gateway(), SUFFIX);
        let site_host = format!("{SITE}.{SUFFIX}");
        // On the gateway's own name everything is as before.
        assert_eq!(ask(app.clone(), "GET", "gateway.example.com", "/storage/v1/objects").await.1, "api read");
        assert_eq!(ask(app.clone(), "POST", "gateway.example.com", "/storage/v1/objects").await.1, "api write");
        let (_, body) = ask(app.clone(), "GET", "gateway.example.com", &format!("/s/{SITE}/a.css")).await;
        assert_eq!(body, format!("site /s/{SITE}/a.css marked="));
        // On a site host a write is refused before any route sees it...
        for uri in ["/storage/v1/objects", "/swarm/v1/run", "/", &format!("/s/{SITE}/x")] {
            let (status, body) = ask(app.clone(), "POST", &site_host, uri).await;
            assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, "not found"), "POST {uri}");
            assert_eq!(ask(app.clone(), "DELETE", &site_host, uri).await.0, StatusCode::NOT_FOUND);
        }
        // ...and a read of an API path is a read of that path of the site.
        let (status, body) = ask(app.clone(), "GET", &site_host, "/storage/v1/objects?list=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("site /s/{SITE}/storage/v1/objects?list=1 marked={SITE}"));
        // Another site's /s/ path is a path of this site too, not that site.
        let other = "ffffffffffffffffffff";
        let (_, body) = ask(app.clone(), "GET", &site_host, &format!("/s/{other}/secret")).await;
        assert_eq!(body, format!("site /s/{SITE}/s/{other}/secret marked={SITE}"));
        let (_, body) = ask(app.clone(), "GET", &format!("{SITE}.{SUFFIX}:443"), "/a%20b.html").await;
        assert_eq!(body, format!("site /s/{SITE}/a%20b.html marked={SITE}"));
        // Wrapped twice, the path is still moved once.
        let twice = isolate_with(app.clone(), SUFFIX);
        let (_, body) = ask(twice, "GET", &site_host, "/a.css").await;
        assert_eq!(body, format!("site /s/{SITE}/a.css marked={SITE}"));
        // A name that only looks like one is the gateway's.
        let (_, body) = ask(app.clone(), "GET", &format!("x.{SITE}.{SUFFIX}"), "/storage/v1/objects").await;
        assert_eq!(body, "api read");
    }

    #[tokio::test]
    async fn without_a_suffix_nothing_changes_and_the_mark_cannot_be_forged() {
        let plain = wrap(gateway(), None);
        let host = format!("{SITE}.{SUFFIX}");
        if configured().is_none() {
            assert_eq!(ask(plain, "POST", &host, "/storage/v1/objects").await.1, "api write");
        }
        // A request that already carries the mark (only code in this process
        // could add it) loses it on the gateway's own name.
        let app = isolate_with(gateway(), SUFFIX);
        let mut req = axum::http::Request::builder()
            .uri(format!("/s/{SITE}/x"))
            .header("host", "gateway.example.com")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(SiteHost { site: SITE.into() });
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), format!("site /s/{SITE}/x marked="));
    }
}
