---
cip: 011
title: "Client-side encryption and key management"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (`private` tier), DIP-0018 (crypto stack precedent)
depends-on: 007
blocks:
implementation:
estimate: "2–3 weeks"
---

## Summary

Encrypt file content and metadata on the client so storage providers hold only
ciphertext. DIP-0012 names sovereignty as one of the five reasons to choose
c0mpute over R2, and it is the only one of the five that a hyperscaler
structurally cannot match — but it is worthless as a claim until the bytes
leaving the client are actually opaque.

## Motivation

Today every shard is plaintext on a stranger's disk. That is acceptable for
public data and unacceptable for anything else, and it makes "sovereignty" a
marketing word rather than a property.

The crypto stack is already chosen and vendored. DIP-0018's secure-chat plugin
brought in `aes-gcm`, `x25519-dalek`, `ed25519-dalek`, `hkdf`, `argon2`,
`zeroize` — all at workspace level in `Cargo.toml`. This CIP applies existing,
already-reviewed dependencies rather than introducing new ones, which is most of
why it is a 2–3 week phase and not a 6-week one.

## Goals

- File content encrypted client-side before erasure coding.
- File *names* and directory structure encrypted — metadata leaks plenty.
- Per-volume keys derived from the CoinPay DID or a passphrase.
- Key rotation without re-uploading all data.
- Sharing a volume, or a subtree, with another DID.
- Deduplication that still works within a volume.

## Non-goals

- Encrypted compute. A worker transcoding a video needs plaintext; DIP-0012
  already says `private` does not apply to workloads that process content.
- Post-quantum for storage at rest in v1. The dependencies are present via
  DIP-0018; see Open questions.
- Hiding file *sizes* or access patterns. See Out of scope.

## Design

### Key hierarchy

```
DID master key (CoinPay, or passphrase via argon2id)
   │
   ├─ HKDF("c0mpute/storage/volume/" || volume_id)  ──► Volume Key (VK)
   │                                                      │
   │   ├─ HKDF(VK, "content" || generation)  ──► Content Key (CK_g)
   │   ├─ HKDF(VK, "names")                  ──► Name Key (NK)
   │   └─ HKDF(VK, "convergent")             ──► Convergence Secret (CS)
   │
   └─ per-recipient X25519 wrap ──► shared volume access
```

Only the master key is ever stored (in the OS keyring, or derived from a
passphrase). Everything else derives, so there is no key database to lose.

### Content encryption

Each chunk is encrypted **before** erasure coding, so providers hold ciphertext
shards and the RS math is unchanged:

```
nonce      = blake3(chunk_plaintext_hash || generation)[0..12]
ciphertext = AES-256-GCM(CK_generation, nonce, chunk_plaintext)
shard_*    = RS_encode(ciphertext)
```

The manifest records the *ciphertext* hash — which is what the network needs
for integrity — and the inode records the plaintext hash for the client's own
verification. Both checks stay intact end to end.

### Convergent encryption, and its cost

Encrypting identical chunks under a random nonce destroys dedup. Deriving the
nonce from the plaintext hash (above) preserves it — identical plaintext yields
identical ciphertext, so dedup works **within a volume**.

The known weakness of convergent encryption is the confirmation-of-file attack:
someone who guesses a chunk's plaintext can confirm you store it. Mixing the
per-volume `CS` into the derivation limits the attack to holders of the volume
key, at the cost of losing cross-volume dedup.

That trade is right for a `private` tier — cross-volume dedup is a provider-side
saving, and confidentiality is what the tier is sold on. `standard` and `hot`
keep global dedup by not encrypting. So the tier choice and the dedup behaviour
are the same choice, and the docs should say so rather than surprising anyone
with a storage bill.

### Metadata encryption

Filenames leak a great deal. Directory entries are encrypted with `NK` using
AES-SIV (deterministic, so lookup by name works without decrypting a whole
directory):

```
stored_name = base64url(AES-SIV(NK, parent_ino || plaintext_name))
```

Determinism per parent means the same name in different directories encrypts
differently, so structure is not inferable across directories. Inode bodies —
sizes, times, modes — are encrypted with `CK` as ordinary content.

What remains visible to a provider: the shape of the HAMT (roughly, how many
entries exist), object and chunk sizes, and access timing. That is a real
residual leak and should be stated plainly in the docs rather than glossed.

### Key rotation

Rotation bumps `generation` and derives a new `CK`. Existing chunks stay under
their old generation — recorded per-extent — so **rotation is O(1), not a
re-upload of the volume.** New writes use the new key. An explicit
`c0mpute storage rekey --rewrite` re-encrypts everything for the case where the
old key is believed compromised, and that one does cost a full rewrite.

### Sharing

Grant another DID access by wrapping the VK to their X25519 public key and
storing the wrapped blob in the volume's access list:

```
c0mpute storage share vol_7f3a --with did:coinpay:... --mode ro
```

Subtree sharing wraps a key derived at that subtree instead. Revocation
requires rotation with `--rewrite` to be meaningful — anyone who held the old
key may have kept the plaintext. The CLI must say this at revoke time; a
revocation that silently does not revoke is worse than none.

### Performance

AES-256-GCM with AES-NI runs at several GB/s per core, well above the network
path, so encryption is not the bottleneck. It does add CPU on a node that may
be running inference — same concern as FastCDC in CIP-007, same mitigation
(cgroup the mount).

## Acceptance criteria

1. With `--encrypt`, no shard on any provider contains recognisable plaintext
   (grep a known string across every node's chunk store: zero hits).
2. Directory listings on a provider reveal no plaintext filenames.
3. Writing the same 100 MB file twice in one volume stores it once (convergent
   dedup); in two different volumes, twice.
4. `rekey` without `--rewrite` completes in under a second on a 1 TB volume and
   new writes use the new generation while old data still reads.
5. `rekey --rewrite` re-encrypts everything and old-key readers fail.
6. A shared DID can read; after revocation plus rewrite, it cannot.
7. Losing local state and recovering from the DID key alone restores full
   plaintext access.
8. Encrypted throughput is within 10% of unencrypted on the same hardware.
9. Keys are zeroized on drop (`zeroize` derive present on every key type).

## Risks

- **Lost key means lost data, permanently.** No recovery, by design.
  *Mitigation:* mandatory acknowledgement at volume creation; optional
  Shamir-split escrow to N recipient DIDs; loud, repeated documentation.
- **Convergent encryption's confirmation attack.** *Mitigation:* per-volume
  convergence secret; document the residual risk honestly rather than claiming
  it is eliminated.
- **Encrypted metadata makes server-side features impossible.** No server-side
  search, no provider-side dedup across customers, no listing without the key.
  *Mitigation:* accepted — it is the point of the tier.
- **Rolling our own construction.** *Mitigation:* use standard primitives in
  standard modes only; no novel crypto; commission a review before any
  `private`-tier data is accepted from a paying customer.
- **Deterministic name encryption leaks equality of names within a directory.**
  Accepted; AES-SIV is chosen precisely for lookup, and the alternative is
  decrypting whole directories per lookup.

## Estimate

**2–3 weeks.** ~0.5 week key hierarchy and derivation, 0.5 week content
encryption in the chunk pipeline, 1 week metadata and name encryption with
lookup, 0.5 week sharing and rotation, 0.5 week tests and adversarial review.

## Out of scope

- Hiding file sizes (would need padding, and it is expensive).
- Hiding access patterns (would need an ORAM-shaped design).
- Hiding volume existence or total size from providers.

## Open questions

- Post-quantum wrapping for shared keys? DIP-0018 already brought a hybrid
  classical+PQ stack in for secure-chat; matching it here is mostly plumbing and
  matters for data with a long confidentiality horizon.
- Should `private` be the default tier rather than opt-in? It costs dedup and
  makes support harder, but "encrypted by default" is a much better promise.
- Where does the master key live on a headless worker with no OS keyring?
