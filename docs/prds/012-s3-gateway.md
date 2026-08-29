---
cip: 012
title: "S3-compatible gateway"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012, DIP-0013 (BYOS positioning)
depends-on: 004
blocks:
implementation:
estimate: "2–3 weeks"
---

## Summary

Speak enough of the S3 API that existing tools — `aws s3`, `rclone`, `s3fs`,
every SDK, every backup product — work against a c0mpute volume with only an
endpoint change. This is the cheapest distribution the storage product can buy.

## Motivation

`docs/storage-pricing.csv` lists `api_style` for every competitor. Almost all of
them say **S3**, including Storj, Wasabi, B2, Hetzner, and Filebase. S3 is the
lingua franca of object storage, and a network storage product without it asks
every prospective customer to write an integration before they can evaluate it.

It is also strictly less risky than the mount: no POSIX semantics, no
concurrent-writer problem (S3 has never promised more than last-writer-wins),
no kernel involvement. It depends only on CIP-004, so it can be built **in
parallel with the entire filesystem track** and shipped first if the mount
slips.

DIP-0013 says BYOS3 is the default and c0mpute storage is opt-in. Speaking S3
makes "opt in" a one-line config change rather than a project.

## Goals

- The S3 operations real tools actually use.
- SigV4 authentication, mapped to CoinPay DIDs.
- Multipart upload for large objects.
- Presigned URLs for browser upload/download.
- Interoperability with the same volumes the mount uses.

## Non-goals

- Complete S3 API coverage. Versioning, lifecycle rules, replication,
  object-lock, inventory, analytics, website hosting, and event notifications
  are all out.
- IAM policy semantics. Access is per-volume by DID, not policy documents.
- Byte-compatible error XML for every edge case; correct codes for the common
  ones.

## Design

### Mapping

| S3 concept | c0mpute |
|---|---|
| Bucket | Volume (CIP-004) |
| Key | Path within the volume |
| Object | File inode + extents (CIP-007) |
| ETag | blake3 hash, hex (**not** MD5 — see below) |
| Storage class | Tier: `STANDARD`→`standard`, `REDUCED_REDUNDANCY`→`hot`, `GLACIER`→rejected |

Buckets and volumes being the same thing is what makes the gateway and the
mount interoperable: write via `aws s3 cp`, read through the FUSE mount, and
vice versa. That is a genuinely useful property and worth protecting in tests.

### Operations

```
Service   ListBuckets
Bucket    CreateBucket, DeleteBucket, HeadBucket, ListObjectsV2, ListObjects
Object    GetObject (+ Range), PutObject, HeadObject, DeleteObject,
          DeleteObjects, CopyObject
Multipart CreateMultipartUpload, UploadPart, CompleteMultipartUpload,
          AbortMultipartUpload, ListParts, ListMultipartUploads
Presign   GET and PUT
```

That set covers `aws s3 sync`, `rclone`, `s3fs`, restic, and the major SDKs.

### The ETag problem

S3 clients expect an ETag that is the MD5 of the object (for single-part
uploads), and some verify it. We hash with blake3 everywhere and have no reason
to compute MD5 over every byte we store.

- Return the blake3 hex as the ETag, which is opaque to well-behaved clients.
- Compute MD5 lazily **only** when a client sends `Content-MD5` or
  `x-amz-content-sha256` and expects verification, and cache it in the inode's
  xattrs.
- Multipart ETags already use S3's `<hash>-<partcount>` form, which no client
  can interpret as a plain MD5, so mimic that shape for multipart.

Document it. `rclone` and the AWS CLI are fine with opaque ETags; a minority of
tools that recompute MD5 client-side will complain, and that is a known,
bounded incompatibility rather than a surprise.

### Authentication

SigV4 with the access key ID being a DID-derived identifier and the secret
being a per-volume derived API secret:

```
c0mpute storage credentials create vol_7f3a --mode rw
  access_key_id:     C0MP7F3A9C2EEXAMPLE
  secret_access_key: ...
  endpoint:          https://<gateway-host>/s3
```

SigV4 verification is standard; the credential lookup resolves to a DID and a
volume, and every request is authorised against that pair. Presigned URLs use
the same secret with S3's standard query-parameter scheme, so browser upload
flows work unmodified.

### Multipart upload

Maps cleanly onto the extent model (CIP-007): each part is chunked and uploaded
independently, then `CompleteMultipartUpload` assembles the extent tree in part
order. Parts are staged as ordinary content-addressed chunks, so an aborted
upload leaves only unreferenced chunks for CIP-004's GC.

Minimum part size 5 MiB, matching S3, except the final part.

### Consistency

S3 has promised read-after-write consistency since 2020, and CIP-004's atomic
root advance provides it naturally — an object is invisible until the root
advances, then fully visible.

`ListObjectsV2` reads a single snapshot, so a listing is a consistent
point-in-time view. That is *stronger* than S3 and costs nothing given the
design.

Concurrent `PutObject` to the same key is last-writer-wins, as in S3. Note this
does **not** require CIP-010's write lease: object writes touch disjoint keys
and the root advance is a compare-and-set, so a losing writer simply retries.
Only the POSIX mount needs exclusive leases. This is why the S3 track can ship
independently, and it is worth stating clearly because it looks like a
contradiction otherwise.

### Deployment

Runs in the existing axum gateway, on a `/s3` prefix or a dedicated port, so it
inherits the current TLS and operational setup. An operator can also run a
public S3 endpoint for their own volumes.

## Acceptance criteria

1. `aws s3 sync ./dir s3://vol_7f3a/prefix/` then `aws s3 sync` back produces
   byte-identical files.
2. `rclone check` between a local dir and the bucket reports zero differences.
3. `restic` initialises a repository, backs up, and `restic check` passes.
4. `s3fs` mounts the bucket and passes basic read/write.
5. A 5 GB multipart upload completes and `HeadObject` reports the right size;
   an aborted multipart leaves no referenced data.
6. Presigned GET and PUT work from a browser with correct CORS.
7. An object written via S3 is visible in the FUSE mount at the same path with
   correct size and mtime, and the reverse.
8. `ListObjectsV2` with 10k keys paginates correctly and each page derives from
   one snapshot.
9. SigV4 with a bad signature returns `SignatureDoesNotMatch`; expired
   presigned URLs return `AccessDenied`.
10. Concurrent PUTs to one key from 10 clients: one wins, no corruption, all
    receive a well-formed response.

## Risks

- **S3 compatibility is a long tail.** Tools depend on undocumented behaviours;
  "S3-compatible" invites bug reports forever. *Mitigation:* publish the
  supported operation list explicitly, test against the four named tools, and
  treat anything outside the list as unsupported rather than broken.
- **ETag/MD5 mismatch breaks a minority of clients.** *Mitigation:* lazy MD5 on
  demand, documented.
- **Latency.** S3 clients expect sub-100 ms; CIP-001 budgets 200–500 ms for
  p2p reads. Some tools' default timeouts and retry behaviour will suffer.
  *Mitigation:* aggressive metadata caching; document recommended timeouts;
  gateway-side read-ahead for sequential `GetObject`.
- **Anonymous public buckets invite abuse.** *Mitigation:* no public-read ACLs
  in v1; presigned URLs only. DIP-0016's abuse policy applies to anything
  publicly served.

## Estimate

**2–3 weeks.** ~0.5 week SigV4 and credentials, 1 week core object operations
and listing, 0.5 week multipart, 0.5 week presigned URLs and CORS, 0.5 week
interop testing against the named tools.

## Open questions

- Should the gateway be a separate binary/service, or a role of the existing
  daemon? A role is simpler; a separate service scales independently and is
  easier to put behind a CDN.
- Bucket naming: expose raw volume ids (ugly, stable) or user-chosen names
  (nice, needs a namespace and collision handling)?
- Is `GLACIER` worth mapping to a future cold tier, or is rejecting it cleaner
  given CIP-001 rules cold storage out?
