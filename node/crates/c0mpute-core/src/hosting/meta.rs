//! Social-card meta tags on hosted HTML.
//!
//! Open Graph and Twitter scrapers do not resolve relative URLs, and a site
//! cannot know which gateway or domain it will be served from, so authors
//! write `<meta property="og:image" content="img/hero.png">` and the link
//! preview comes out blank. The gateway knows the public URL of every
//! request, so it rewrites those tags to absolute URLs on the way out.

use axum::http::{HeaderMap, header};

/// Meta tags whose `content` is a URL scrapers fetch.
const URL_META: &[&str] = &[
    "og:image",
    "og:image:url",
    "og:image:secure_url",
    "og:url",
    "og:audio",
    "og:video",
    "og:video:url",
    "og:video:secure_url",
    "twitter:image",
    "twitter:image:src",
    "twitter:player",
];

/// The public origin of a request (`https://gateway.example`), from the
/// proxy's `X-Forwarded-*` headers when present, else `Host`. Plain http
/// only for localhost and bare IPs; every public gateway is behind TLS.
pub fn public_origin(headers: &HeaderMap) -> Option<String> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let host = get("x-forwarded-host").or_else(|| get(header::HOST.as_str()))?;
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        return None;
    }
    let bare = host.rsplit_once(':').map_or(host.as_str(), |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host.as_str()
        }
    });
    let local = bare == "localhost"
        || bare.ends_with(".localhost")
        || bare
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok();
    let scheme = match get("x-forwarded-proto").as_deref() {
        Some("http") => "http",
        Some("https") => "https",
        _ if local => "http",
        _ => "https",
    };
    Some(format!("{scheme}://{host}"))
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<(usize, usize, &'a str)> {
    // Returns (value start, value end, value) within `tag`.
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = &lower[from..];
        let eq = rest.len() - rest.trim_start().len();
        if !before_ok || !rest.trim_start().starts_with('=') {
            continue;
        }
        let mut v = from + eq + 1;
        while tag.as_bytes().get(v).is_some_and(u8::is_ascii_whitespace) {
            v += 1;
        }
        let quote = tag.as_bytes().get(v).copied();
        return match quote {
            Some(q @ (b'"' | b'\'')) => {
                let start = v + 1;
                let end = start + tag[start..].find(q as char)?;
                Some((start, end, &tag[start..end]))
            }
            Some(_) => {
                let mut end = v + tag[v..]
                    .find(|c: char| c.is_ascii_whitespace() || c == '>')
                    .unwrap_or(tag.len() - v);
                // `content=x.png/>`: the slash closes the tag.
                if tag[end..].starts_with('>') && tag[v..end].ends_with('/') {
                    end -= 1;
                }
                Some((v, end, &tag[v..end]))
            }
            None => None,
        };
    }
    None
}

fn is_absolute(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://") || u.starts_with("data:")
}

/// Rewrite relative URL meta tags in `html` against `page_url` (the absolute
/// URL the document was served at). `None` when nothing needed changing.
pub fn absolutize(html: &str, page_url: &url::Url) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    // Only the head matters; stop at <body> to keep big pages cheap.
    let limit = lower.find("<body").unwrap_or(lower.len());
    let mut out = String::with_capacity(html.len() + 128);
    let mut copied = 0;
    let mut changed = false;
    let mut from = 0;
    while let Some(i) = lower[from..limit].find("<meta") {
        let start = from + i;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        let end = start + len + 1;
        from = end;
        let tag = &html[start..end];
        let key = attr(tag, "property").or_else(|| attr(tag, "name"));
        let Some((_, _, key)) = key else { continue };
        if !URL_META.contains(&key.trim().to_ascii_lowercase().as_str()) {
            continue;
        }
        let Some((vs, ve, value)) = attr(tag, "content") else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() || is_absolute(value) {
            continue;
        }
        let decoded = value.replace("&amp;", "&");
        let Ok(joined) = page_url.join(&decoded) else {
            continue;
        };
        let escaped = joined.as_str().replace('&', "&amp;").replace('"', "%22");
        out.push_str(&html[copied..start + vs]);
        out.push_str(&escaped);
        copied = start + ve;
        changed = true;
    }
    if !changed {
        return None;
    }
    out.push_str(&html[copied..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn page(u: &str) -> url::Url {
        url::Url::parse(u).unwrap()
    }

    #[test]
    fn relative_og_image_becomes_absolute() {
        let html = r#"<head><meta property="og:title" content="Hi"><meta property="og:image" content="img/hero.png"><meta name="twitter:image" content='/a.png?x=1&amp;y=2'></head><body><meta property="og:image" content="z.png"></body>"#;
        let out = absolutize(html, &page("https://gw.example/s/abc/")).unwrap();
        assert!(
            out.contains(r#"content="https://gw.example/s/abc/img/hero.png""#),
            "{out}"
        );
        assert!(
            out.contains("content='https://gw.example/a.png?x=1&amp;y=2'"),
            "{out}"
        );
        assert!(out.contains(r#"content="Hi""#));
        // Past <body>: untouched.
        assert!(out.contains(r#"content="z.png""#));
    }

    #[test]
    fn absolute_and_unrelated_tags_are_left_alone() {
        let html = r#"<meta property="og:image" content="https://cdn.example/x.png"><meta name="description" content="img/no.png"><link rel="icon" href="i.png">"#;
        assert!(absolutize(html, &page("https://gw.example/s/abc/")).is_none());
        let html = r#"<META PROPERTY=og:image CONTENT=hero.png>"#;
        assert_eq!(
            absolutize(html, &page("https://site.example/blog/post.html")).unwrap(),
            "<META PROPERTY=og:image CONTENT=https://site.example/blog/hero.png>"
        );
    }

    #[test]
    fn origins() {
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("gateway.c0mpute.com"),
        );
        assert_eq!(
            public_origin(&h).as_deref(),
            Some("https://gateway.c0mpute.com")
        );
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert_eq!(
            public_origin(&h).as_deref(),
            Some("http://gateway.c0mpute.com")
        );
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7777"));
        assert_eq!(public_origin(&h).as_deref(), Some("http://127.0.0.1:7777"));
        h.insert(
            "x-forwarded-host",
            HeaderValue::from_static("site.example, proxy"),
        );
        assert_eq!(public_origin(&h).as_deref(), Some("https://site.example"));
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("evil\"host"));
        assert_eq!(public_origin(&h), None);
        assert_eq!(public_origin(&HeaderMap::new()), None);
    }
}
