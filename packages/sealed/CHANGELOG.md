# Changelog

## 0.1.0

- First release: the `c0mpute-private-v1` sealed format in JavaScript
  (WebCrypto AES-256-GCM STREAM, X25519 key wrap, Ed25519 owner signature,
  BLAKE3, Padmé), streaming over a Blob so large files are read in slices.
- Custody: each object's signing key is wrapped to the owner, so the owner's
  `c0mpute storage share|revoke|rm` work on objects an app uploaded.
- Client for c0mpute.com's `storage:write` API: `identity`, `linkStatus`,
  `uploadSealed` (resumable 8 MiB pieces to the ingest gateway), `status`,
  `link`.
- Objects made here open with the Rust CLI (`c0mpute storage get`), checked
  in CI (`sealed-js` workflow).
