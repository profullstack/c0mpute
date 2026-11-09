//! HTTP to an app, from any gateway, over libp2p.
//!
//! A gateway that is not the app's primary wraps the request in an
//! [`HttpReq`] and sends it to the primary over the `apps.http` RPC (which
//! dials through NAT the same way chunk fetches do). The primary forwards it
//! to its container on loopback and returns an [`HttpResp`]. Bodies are
//! buffered and capped: this carries APIs, form posts and pages, not uploads
//! of gigabytes or long-lived streams (websockets are not proxied).

use std::time::Duration;

use anyhow::{Result, bail};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Largest request body carried to an app (libp2p's request cap is 1 MiB,
/// and the JSON envelope base64s the body).
pub const MAX_REQUEST_BODY: usize = 700 * 1024;
/// Largest response body carried back (the RPC response cap is 8 MiB).
pub const MAX_RESPONSE_BODY: usize = 5 * 1024 * 1024;
/// How long the primary waits on its container.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpReq {
    pub app_id: String,
    pub method: String,
    /// Path and query as the app should see them (`/v1/items?x=1`).
    pub path: String,
    pub headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes_b64")]
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes_b64")]
    pub body: Vec<u8>,
}

mod serde_bytes_b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

/// Hop-by-hop and spoofable headers that never cross the proxy.
fn dropped(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-real-ip"
            | "forwarded"
    )
}

/// Build the envelope a gateway sends. `client_ip` is what the gateway saw.
pub fn wrap(
    app_id: &str,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
    host: Option<&str>,
    proto: &str,
    client_ip: Option<&str>,
) -> Result<HttpReq> {
    if body.len() > MAX_REQUEST_BODY {
        bail!("request body over {MAX_REQUEST_BODY} bytes");
    }
    let mut h: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !dropped(k.as_str()))
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .take(100)
        .collect();
    if let Some(ip) = client_ip {
        h.push(("x-forwarded-for".into(), ip.to_string()));
    }
    if let Some(host) = host {
        h.push(("x-forwarded-host".into(), host.to_string()));
    }
    h.push(("x-forwarded-proto".into(), proto.to_string()));
    Ok(HttpReq {
        app_id: app_id.to_string(),
        method: method.as_str().to_string(),
        path: if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        },
        headers: h,
        body,
    })
}

/// Primary side: send the envelope to the container on loopback.
pub async fn forward_local(host_port: u16, req: &HttpReq) -> HttpResp {
    match forward_inner(host_port, req).await {
        Ok(r) => r,
        Err(e) => HttpResp {
            status: 502,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: format!("app did not answer: {e}").into_bytes(),
        },
    }
}

async fn forward_inner(host_port: u16, req: &HttpReq) -> Result<HttpResp> {
    if !req.path.starts_with('/') || req.path.contains(['\r', '\n']) {
        bail!("bad path");
    }
    let method = Method::from_bytes(req.method.as_bytes())?;
    let client = reqwest::Client::builder()
        .timeout(UPSTREAM_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut rb = client.request(method, format!("http://127.0.0.1:{host_port}{}", req.path));
    for (k, v) in &req.headers {
        if !dropped(k) || k.starts_with("x-forwarded-") {
            rb = rb.header(k.as_str(), v.as_str());
        }
    }
    let res = rb.body(req.body.clone()).send().await?;
    let status = res.status().as_u16();
    let headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .filter(|(k, _)| !dropped(k.as_str()))
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let mut body = Vec::new();
    let mut res = res;
    while let Some(chunk) = res.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BODY {
            bail!("response over {MAX_RESPONSE_BODY} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HttpResp {
        status,
        headers,
        body,
    })
}

/// Turn the envelope back into an axum response at the gateway.
pub fn into_response(r: HttpResp) -> Response {
    let mut resp = Response::new(Body::from(r.body));
    *resp.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let h = resp.headers_mut();
    for (k, v) in r.headers {
        if dropped(&k) {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            h.append(k, v);
        }
    }
    h.insert("x-served-by", HeaderValue::from_static("c0mpute-apps"));
    resp
}

pub fn unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            ("retry-after", "10"),
            ("content-type", "text/plain; charset=utf-8"),
        ],
        msg.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_strips_hop_by_hop_and_spoofed_forwarding() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "6.6.6.6".parse().unwrap());
        h.insert("connection", "upgrade".parse().unwrap());
        h.insert("authorization", "Bearer t".parse().unwrap());
        h.insert("host", "evil".parse().unwrap());
        let r = wrap(
            "a",
            &Method::POST,
            "v1/x?y=1",
            &h,
            b"{}".to_vec(),
            Some("api.example"),
            "https",
            Some("1.2.3.4"),
        )
        .unwrap();
        assert_eq!(r.path, "/v1/x?y=1");
        let get = |k: &str| {
            r.headers
                .iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            get("x-forwarded-for"),
            vec!["1.2.3.4"],
            "the client's own header is replaced"
        );
        assert!(get("connection").is_empty() && get("host").is_empty());
        assert_eq!(get("authorization"), vec!["Bearer t"]);
        assert_eq!(get("x-forwarded-host"), vec!["api.example"]);
        let json = serde_json::to_vec(&r).unwrap();
        let back: HttpReq = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.body, b"{}");
        assert!(
            wrap(
                "a",
                &Method::POST,
                "/",
                &h,
                vec![0; MAX_REQUEST_BODY + 1],
                None,
                "http",
                None
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn forwards_to_a_loopback_server() {
        let app = axum::Router::new().route(
            "/echo",
            axum::routing::post(|headers: HeaderMap, body: String| async move {
                let fwd = headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                (
                    StatusCode::CREATED,
                    [("x-app", "yes")],
                    format!("{fwd}|{body}"),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let req = wrap(
            "a",
            &Method::POST,
            "/echo",
            &HeaderMap::new(),
            b"hi".to_vec(),
            None,
            "http",
            Some("9.9.9.9"),
        )
        .unwrap();
        let r = forward_local(port, &req).await;
        assert_eq!(r.status, 201);
        assert_eq!(r.body, b"9.9.9.9|hi");
        assert!(r.headers.iter().any(|(k, v)| k == "x-app" && v == "yes"));
        let dead = forward_local(1, &req).await;
        assert_eq!(dead.status, 502);
    }
}
