//! DNS-rebinding defense for HTTP surfaces bound to loopback.
//!
//! A service on `127.0.0.1` is reachable from any web page the operator
//! opens: the page's origin resolves its own hostname to 127.0.0.1 a second
//! time (DNS rebinding), and the browser then treats the local service as
//! same-origin. The `Host` header still carries the attacker's name, so a
//! loopback service that only answers `127.0.0.1:<port>`, `localhost:<port>`
//! or `[::1]:<port>` cannot be driven that way.
//!
//! [`guard_loopback`] is a no-op for a listener on any other address (public
//! gateways serve many hostnames). A loopback service fronted by a reverse
//! proxy that forwards the public `Host` lists those names in
//! `C0MPUTE_ALLOWED_HOSTS` (comma-separated, with or without a port).

use std::net::SocketAddr;

use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};

/// Env var listing extra `Host` values a loopback service accepts.
pub const ALLOWED_HOSTS_ENV: &str = "C0MPUTE_ALLOWED_HOSTS";

/// Whether `host` (a `Host` header value) may reach a loopback service on
/// `port`. `extra` holds operator-allowed names (`name` or `name:port`).
pub fn host_allowed(host: Option<&str>, port: u16, extra: &[String]) -> bool {
    let Some(host) = host.map(|h| h.trim().to_ascii_lowercase()) else {
        // HTTP/1.0 clients may omit Host; a browser never does.
        return true;
    };
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
    if local.iter().any(|l| *l == host) {
        return true;
    }
    // Port 80 may be implied.
    if port == 80 && matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return true;
    }
    // The name without its `:port`, so an allowed bare name matches any port.
    let bare = match host.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => host.clone(),
    };
    extra.iter().map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).any(|e| e == host || e == bare)
}

fn extra_from_env() -> Vec<String> {
    std::env::var(ALLOWED_HOSTS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Wrap `app` with the `Host` check when `addr` is a loopback address.
pub fn guard_loopback(app: Router, addr: SocketAddr) -> Router {
    if !addr.ip().is_loopback() {
        return app;
    }
    let port = addr.port();
    let extra = extra_from_env();
    app.layer(middleware::from_fn(move |req: Request, next: Next| {
        let extra = extra.clone();
        async move {
            let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok());
            let host = host.or_else(|| req.uri().authority().map(|a| a.as_str()));
            if !host_allowed(host, port, &extra) {
                return refuse();
            }
            next.run(req).await
        }
    }))
}

fn refuse() -> Response {
    (
        StatusCode::MISDIRECTED_REQUEST,
        "this local c0mpute service answers only 127.0.0.1, localhost or [::1] (set C0MPUTE_ALLOWED_HOSTS for a proxy)\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::get};
    use tower::ServiceExt;

    #[test]
    fn allows_only_local_names_on_the_right_port() {
        assert!(host_allowed(Some("127.0.0.1:8788"), 8788, &[]));
        assert!(host_allowed(Some("LOCALHOST:8788"), 8788, &[]));
        assert!(host_allowed(Some("[::1]:8788"), 8788, &[]));
        assert!(host_allowed(None, 8788, &[]));
        assert!(!host_allowed(Some("evil.example:8788"), 8788, &[]), "rebinding name refused");
        assert!(!host_allowed(Some("127.0.0.1:9999"), 8788, &[]), "wrong port refused");
        assert!(!host_allowed(Some("localhost"), 8788, &[]));
        assert!(host_allowed(Some("localhost"), 80, &[]));
        let extra = vec!["speech.example.com".to_string(), "api.example.com:443".to_string()];
        assert!(host_allowed(Some("speech.example.com"), 8788, &extra));
        assert!(host_allowed(Some("speech.example.com:8443"), 8788, &extra));
        assert!(host_allowed(Some("api.example.com:443"), 8788, &extra));
        assert!(!host_allowed(Some("other.example.com"), 8788, &extra));
    }

    async fn status(app: Router, host: &str) -> StatusCode {
        let req = axum::http::Request::builder().uri("/").header("host", host).body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn loopback_listener_refuses_foreign_hosts_public_one_does_not() {
        let app = Router::new().route("/", get(|| async { "ok" }));
        let lo: SocketAddr = "127.0.0.1:8788".parse().unwrap();
        assert_eq!(status(guard_loopback(app.clone(), lo), "127.0.0.1:8788").await, StatusCode::OK);
        assert_eq!(status(guard_loopback(app.clone(), lo), "evil.example:8788").await, StatusCode::MISDIRECTED_REQUEST);
        let public: SocketAddr = "0.0.0.0:7777".parse().unwrap();
        assert_eq!(status(guard_loopback(app, public), "gateway.example").await, StatusCode::OK);
    }
}
