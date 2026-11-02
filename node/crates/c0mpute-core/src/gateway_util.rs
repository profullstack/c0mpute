//! Pieces shared by the HTTP buyer bridges (`c0mpute speech serve`,
//! `c0mpute jobs serve`): bearer-token checks and fetching a worker's result
//! by the hash its receipt named.

use std::time::Duration;

use axum::http::{HeaderMap, header};
use c0mpute_net::{Libp2pNetwork, Network};
use c0mpute_proto::{ChunkRequest, Hash};

/// Compare two secrets without an early exit, so response timing does not
/// reveal how much of a guessed token was right.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        difference |=
            usize::from(a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

/// Outcome of a bearer check, kept apart so callers word their own errors.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BearerCheck {
    Ok,
    Missing,
    Invalid,
}

pub(crate) fn check_bearer(headers: &HeaderMap, token: &str) -> BearerCheck {
    let Some(supplied) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return BearerCheck::Missing;
    };
    if constant_time_eq(supplied.as_bytes(), token.as_bytes()) {
        BearerCheck::Ok
    } else {
        BearerCheck::Invalid
    }
}

/// Why a worker's result could not be retrieved.
#[derive(Debug)]
pub(crate) enum FetchError {
    /// The receipt's hash or peer id is malformed, or the bytes that came
    /// back do not hash to it, or exceed the size cap.
    Invalid(&'static str),
    /// Nobody served the result before the deadline.
    Timeout(String),
}

/// Fetch `hash` from the winning worker (direct dial first, then a DHT
/// provider lookup, then direct retries) and verify the bytes hash to it.
pub(crate) async fn fetch_verified(
    net: &Libp2pNetwork,
    hash: &str,
    provider_peer_id: &str,
    max_bytes: usize,
    budget: Duration,
) -> Result<Vec<u8>, FetchError> {
    let hash = Hash::from_hex(hash)
        .map_err(|_| FetchError::Invalid("worker returned an invalid result hash"))?;
    let provider: c0mpute_net::PeerId = provider_peer_id
        .parse()
        .map_err(|_| FetchError::Invalid("worker returned an invalid peer id"))?;
    let request = ChunkRequest {
        chunk_hash: hash,
        shard_index: None,
    };
    let validate = |bytes: Vec<u8>| {
        if Hash::of(&bytes) == hash && bytes.len() <= max_bytes {
            Ok(bytes)
        } else {
            Err(FetchError::Invalid("worker result hash or size is invalid"))
        }
    };

    let start = tokio::time::Instant::now();
    let deadline = start + budget;
    let direct_deadline = (start + Duration::from_secs(3)).min(deadline);
    let direct_error =
        match tokio::time::timeout_at(direct_deadline, net.fetch_from_peer(provider, &request))
            .await
        {
            Ok(Ok(bytes)) => return validate(bytes),
            Ok(Err(error)) => error.to_string(),
            Err(_) => "direct fetch timed out".into(),
        };

    // A job receipt can arrive before the provider record has reached this
    // node's routing table. Try the normal DHT provider lookup before giving
    // up on a worker whose result is otherwise complete.
    let dht_deadline = (tokio::time::Instant::now() + Duration::from_secs(12)).min(deadline);
    let mut last_error = match tokio::time::timeout_at(dht_deadline, net.fetch(&request)).await {
        Ok(Ok(bytes)) => return validate(bytes),
        Ok(Err(error)) => format!("direct fetch: {direct_error}; DHT fetch: {error}"),
        Err(_) => format!("direct fetch: {direct_error}; DHT provider fetch timed out"),
    };

    while tokio::time::Instant::now() < deadline {
        let retry_deadline = (tokio::time::Instant::now() + Duration::from_secs(2)).min(deadline);
        match tokio::time::timeout_at(retry_deadline, net.fetch_from_peer(provider, &request)).await
        {
            Ok(Ok(bytes)) => return validate(bytes),
            Ok(Err(error)) => last_error = error.to_string(),
            Err(_) => last_error = "direct fetch timed out".into(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(FetchError::Timeout(last_error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_secrets() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn bearer_check() {
        let mut h = HeaderMap::new();
        assert_eq!(check_bearer(&h, "secret"), BearerCheck::Missing);
        h.insert(header::AUTHORIZATION, "Bearer nope".parse().unwrap());
        assert_eq!(check_bearer(&h, "secret"), BearerCheck::Invalid);
        h.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(check_bearer(&h, "secret"), BearerCheck::Ok);
        h.insert(header::AUTHORIZATION, "Basic secret".parse().unwrap());
        assert_eq!(check_bearer(&h, "secret"), BearerCheck::Missing);
    }
}
