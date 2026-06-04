---
dip: 0018
title: "secure-chat — end-to-end encrypted p2p messaging plugin"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-06-04
updated: 2026-06-04
discussion:
implementation:
supersedes:
superseded-by:
---

## Summary

`secure-chat` is a first-party c0mpute plugin that adds end-to-end encrypted
peer-to-peer messaging across the c0mpute network. No central server stores
messages or key material. Every user holds their own keypair; public keys are
distributed via the network's DHT gossip layer; private keys are encrypted with
a user-supplied password and returned as a local backup file. Identity and
reputation anchor to the existing CoinPay DID system (DIP-0007). The plugin
surfaces as `c0mpute chat` in the CLI, a web UI at `c0mpute.com/chat`, and
optionally inside the TUI.

The same plugin serves all verticals — transcode operators, infernet workers,
hosting providers, buyers — anywhere two actors on the c0mpute network need to
communicate without an intermediary reading or storing their messages.

## Motivation

Today there is no first-class channel for actors on the c0mpute network to
communicate. Workers and buyers negotiate job terms, dispute receipts, and
coordinate outside the network (Telegram, Discord, email) — all centralized,
all deanonymizing. This creates three concrete problems:

1. **Coordination leaks.** Job negotiation happening off-network breaks the
   "no central dependency" guarantee of DIP-0011.
2. **No reputation-linked messaging.** A spammer or scammer can contact any
   worker without any identity cost. Tying messages to CoinPay DIDs gives
   reputation skin in the game.
3. **No native channel for plugin events.** Transcode job completion
   notifications, infernet inference receipts, and hosting proof-of-serve
   disputes all need a delivery channel. Right now there is none.

Building this as a plugin (not a core feature) means it ships independently,
other networks can adopt the protocol, and the implementation cost falls on the
plugin release cycle rather than the core binary.

## Detailed design

### 1. Cryptographic primitives

| Purpose | Algorithm |
|---|---|
| Key derivation (seed → keypairs) | HKDF-SHA256 from a 32-byte random seed |
| Encryption keypair | X25519 (Curve25519 ECDH) |
| Signing keypair | Ed25519 |
| Message encryption | NaCl `crypto_box` (X25519 + XSalsa20-Poly1305) |
| Symmetric group key encryption | XChaCha20-Poly1305 |
| Private key backup encryption | Argon2id → AES-256-GCM |
| Key fingerprint | SHA-256 truncated to 16 bytes, displayed as 8 hex pairs |

Both keypairs are derived from a single 32-byte seed so the user only ever
backs up one secret. The seed itself is never stored unencrypted.

### 2. Key lifecycle

#### 2a. Key generation

```
c0mpute chat keygen
```

Steps:
1. Generate 32 random bytes → `seed`
2. Derive X25519 encryption keypair via `HKDF(seed, "secure-chat-enc")`
3. Derive Ed25519 signing keypair via `HKDF(seed, "secure-chat-sig")`
4. Prompt for password (twice; min 10 chars)
5. `Argon2id(password, random_salt, t=3, m=65536, p=4)` → `kek` (key-encryption key)
6. `AES-256-GCM(kek, nonce, seed)` → `ciphertext`
7. Write `~/.config/c0mpute/chat.key` (JSON, chmod 600):

```json
{
  "v": 1,
  "alg": "argon2id-aes256gcm",
  "argon2": { "t": 3, "m": 65536, "p": 4, "salt": "<base64url>" },
  "enc_nonce": "<base64url>",
  "ciphertext": "<base64url>",
  "pubkey_enc": "<base64url(X25519 pubkey)>",
  "pubkey_sig": "<base64url(Ed25519 pubkey)>",
  "did": "did:coinpay:user:<id>",
  "created_at": 1748995200
}
```

8. The `ciphertext` field alone is the backup blob — output to stdout and
   prompt the user to save it:

```
Your encrypted key backup (save this somewhere safe):

{...full JSON...}

To restore: c0mpute chat restore --from-backup <file>
```

The backup file contains no plaintext secret. Compromise of the file without
the password reveals only the public keys (which are already public).

#### 2b. Private key unlock (session)

```
c0mpute chat unlock
```

Reads `~/.config/c0mpute/chat.key`, prompts for password, decrypts seed into
memory (or a locked unix socket like coinpay daemon). Private key material never
touches disk unencrypted after keygen.

#### 2c. Key restoration

```
c0mpute chat restore --from-backup <file.json>
```

Reads the backup JSON, prompts for password, verifies decryption succeeds,
writes to `~/.config/c0mpute/chat.key`.

### 3. Public key distribution

Public keys are distributed via the c0mpute DHT (the same gossip layer that
propagates node capacity and plugin announcements). No dedicated key server.

#### Announcement record

```json
{
  "v": 1,
  "did": "did:coinpay:user:abc123",
  "pubkey_enc": "<base64url(X25519)>",
  "pubkey_sig": "<base64url(Ed25519)>",
  "endpoints": ["wss://node1.c0mpute.com/chat", "..."],
  "created_at": 1748995200,
  "expires_at":  1749081600,
  "sig": "<Ed25519 sig over canonical JSON>"
}
```

DHT key: `SHA256("secure-chat-key:" + did)` (32 bytes, standard Kademlia lookup).

Each node that has the `secure-chat` plugin enabled serves DHT lookups and
stores up to N key records (LRU, bounded). Nodes re-propagate records before
`expires_at`. The announcing user re-publishes every 12–24 hours to stay
visible.

Discovery command:

```bash
c0mpute chat lookup did:coinpay:user:abc123
# → prints pubkey fingerprint + endpoints
```

The CoinPay DID document also gains a `SecureChat` service entry (opt-in):

```json
"service": [{
  "id": "#secure-chat",
  "type": "SecureChatKey",
  "serviceEndpoint": "did:coinpay:user:abc123#secure-chat",
  "pubkey_enc": "<base64url>",
  "pubkey_sig": "<base64url>"
}]
```

This makes the public key resolvable via standard DID resolution in addition to
the DHT path.

### 4. Message format

Every message is a signed, encrypted JSON envelope:

```json
{
  "v": 1,
  "id": "<sha256(canonical_fields)>",
  "kind": "dm",
  "from": "did:coinpay:user:abc123",
  "to":   "did:coinpay:user:def456",
  "ciphertext": "<base64url>",
  "enc_nonce":  "<base64url(random 24 bytes)>",
  "created_at": 1748995200,
  "ttl":        86400,
  "sig": "<base64url(Ed25519 sig over id || created_at || to)>"
}
```

`ciphertext` is `crypto_box(plaintext, nonce, recipient_X25519_pubkey, sender_X25519_privkey)`.
The plaintext itself is a JSON object:

```json
{
  "text": "Hey, your transcode job finished!",
  "thread_id": "<optional>",
  "reply_to":  "<optional message id>",
  "attachments": []
}
```

Nodes that relay the envelope see only the outer fields (from DID, to DID,
TTL). They cannot read the content. The from DID is visible to relays; see
§4a for sealed-sender mode.

#### 4a. Sealed-sender mode (privacy tier)

For higher privacy, the sender wraps the entire inner envelope (including `from`
and `sig`) inside a second `crypto_box` encrypted to the recipient only. The
outer envelope then has `from: "anonymous"`. Only the recipient can verify
sender identity. Relays cannot correlate sender to message.

This is opt-in per message: `c0mpute chat send --sealed`.

### 5. Group chats

Group chats use a symmetric group key distributed to members via individual
`crypto_box` envelopes:

1. Creator generates a 32-byte `group_key` and a `group_id = SHA256(group_key || created_at)`.
2. Creator sends each invited member a `kind: "group-invite"` message containing
   `crypto_box(group_key, nonce, member_pubkey, creator_privkey)`.
3. Members that accept store the group key locally, keyed by `group_id`.
4. Group messages use `XChaCha20-Poly1305(plaintext, group_key, random_nonce)`.
5. When a member is removed or leaves, the admin rotates the group key and
   re-encrypts for remaining members.

### 6. Transport (gossip + WebSocket relay)

```
┌─────────┐   send   ┌────────────────────┐  DHT/gossip  ┌────────────────────┐
│ Sender  │ ────────▶│ c0mpute node (any) │─────────────▶│ c0mpute node (any) │
└─────────┘          └────────────────────┘              └────────────────────┘
                                                                    │
                                                           WebSocket push (if online)
                                                                    │
                                                               ┌─────────┐
                                                               │Recipient│
                                                               └─────────┘
```

- **Online delivery**: recipient is connected via WebSocket to a node; node
  pushes immediately, no store-and-forward needed.
- **Offline / store-and-forward**: nodes store encrypted envelopes for up to
  `ttl` seconds (max 7 days, default 24 hours). On reconnect, recipient pulls
  queued messages for their DID.
- **Pull mode**: `c0mpute chat pull` fetches any queued messages from the node
  the user is registered with.
- **Nodes do not index message content** — they store the opaque binary blob
  indexed by `(to_did_hash, message_id)`.

The existing c0mpute gossip/WebSocket infrastructure (used for job dispatch) is
the transport substrate. `secure-chat` registers a message type `0x20` in the
gossip protocol.

### 7. CoinPay DID integration

- **Identity**: sender's `from` DID is verified against the DID document's
  `pubkey_sig` on receipt. No DID → message rejected.
- **Reputation cost**: each message sent increments a lightweight counter
  (`chat.sent`) on the sender's DID. Reported-as-spam increments
  `chat.spam_reports`. If `spam_reports / sent > 0.05` (5%), the DID's
  reputation is flagged and nodes may rate-limit delivery.
- **Block list**: stored locally. `c0mpute chat block did:coinpay:user:xyz` adds
  DID to local blocklist; nodes never push blocked DIDs.
- **Subscription / contact list**: stored locally in
  `~/.config/c0mpute/chat-contacts.json` (plaintext DID list, no message
  content).

### 8. CLI surface

```bash
# Setup
c0mpute chat keygen               # generate keypair, prompt for password
c0mpute chat unlock               # decrypt key into session
c0mpute chat restore --from-backup <file>

# Contacts & discovery
c0mpute chat lookup <did>         # resolve pubkey + endpoints
c0mpute chat add <did> [--name alias]
c0mpute chat contacts

# Messaging
c0mpute chat send <did> "message"
c0mpute chat send --sealed <did>  # sealed-sender mode
c0mpute chat pull                 # fetch queued messages
c0mpute chat read                 # interactive inbox

# Groups
c0mpute chat group create --name "transcode-team" --invite <did1> <did2>
c0mpute chat group send <group_id> "message"
c0mpute chat group add <group_id> <did>
c0mpute chat group leave <group_id>

# Moderation
c0mpute chat block <did>
c0mpute chat report <message_id> --reason spam

# Key management
c0mpute chat key show             # print public key + fingerprint
c0mpute chat key rotate           # generate new keypair, re-announce
c0mpute chat key export           # re-export backup blob
```

### 9. Web UI surface (`c0mpute.com/chat`)

The web surface is a Next.js app (same monorepo pattern as other verticals).
It runs in the browser; private key material is held in `sessionStorage` only
(never sent to any server). WebSocket connection goes directly to a c0mpute
node of the user's choice.

Key screens:
- **Inbox** — chronological list of conversations
- **Compose** — recipient DID or alias, message body, sealed-sender toggle
- **Key setup wizard** — guided keygen / restore flow with password strength meter
- **Key backup prompt** — shown once after keygen with a copy/download button
- **Group management** — create, invite, rotate

### 10. TUI surface

The existing `apps/tui` gets a `chat` panel that subscribes to the WebSocket
push stream from the user's preferred node. Real-time delivery without polling.

### 11. Plugin manifest

See `plugins/secure-chat/module.toml` (added alongside this DIP).

`kind: "service"` — this is a continuous-availability service role, not a
one-shot workload. Nodes that run `secure-chat` accept WebSocket connections,
store-and-forward envelopes within TTL, and participate in DHT key lookups.

### 12. Security considerations

| Threat | Mitigation |
|---|---|
| Node reads message content | Messages encrypted before leaving sender; nodes see only opaque ciphertext |
| Attacker steals key backup file | File contains only Argon2id-encrypted seed; useless without password |
| Replay attack | `message_id = SHA256(from \|\| to \|\| created_at \|\| nonce)` is unique; nodes reject duplicate IDs within TTL window |
| Sender impersonation | Ed25519 sig over message ID verified against sender's DID-registered pubkey |
| Spam / DoS | Messages require valid DID sig; nodes rate-limit by sender DID; reputation cost on reports |
| Metadata correlation (sender visible) | Sealed-sender mode hides from-DID from relays |
| Key compromise | Key rotation command re-announces new pubkey; old pubkey revoked with DID-signed revocation record |
| Node collusion (traffic analysis) | Out of scope for v1; onion routing considered in §Alternatives |
| Weak passwords | Argon2id with high memory cost; minimum password entropy enforced at keygen |

## Alternatives considered

**Signal Protocol (Double Ratchet + X3DH).** Provides forward secrecy and
break-in recovery. The complexity (prekey bundles, ratchet state sync) is
significant and requires persistent state per conversation pair. Good for v2
once the basic transport is proven. Out of scope for v1.

**Nostr DMs (NIP-04 / NIP-44).** Nostr relays are centralized (pick-your-relay).
NIP-04 has known weaknesses (same key for signing and encryption). NIP-44
(XChaCha20, HKDF) is closer to what we want but still relay-dependent. We borrow
the message format shape from Nostr but distribute via our own DHT rather than
choosing a relay.

**Matrix / Element.** Federated, not decentralized. Requires servers. Doesn't
integrate with CoinPay DID. Not a plugin.

**IPFS pubsub.** Poor reliability; no guaranteed delivery; content routing not
optimized for small real-time messages.

**Onion routing for metadata privacy.** Better than sealed-sender but adds
significant latency and implementation complexity. Sealed-sender is a pragmatic
first step; onion routing is a v2 option.

**Separate identity system for chat.** The point of CoinPay DID (DIP-0007) is
to be the single identity layer. Adding a separate chat identity breaks that and
splits reputation.

## Migration & rollout

1. **v0.1 — in-process CLI only.** `c0mpute chat` ships compiled into the
   binary. No node-side relay yet — messages go point-to-point via direct
   WebSocket between two online nodes. Useful for testing the crypto layer.
2. **v0.2 — node relay + store-and-forward.** Nodes that opt-in to
   `secure-chat` service role store envelopes up to TTL. `c0mpute chat pull`
   works. DHT key distribution live.
3. **v0.3 — web UI + TUI panel.** Browser-based inbox. TUI real-time panel.
4. **v0.4 — group chats.** Group key distribution, admin rotation.
5. **v0.5 — sealed-sender mode.** Optional per-message.
6. **v1.0 — Double Ratchet (forward secrecy).** Requires client state; design
   in a follow-on DIP.

No migration from existing users — this is a new plugin with no prior state.
Rollout is gated behind `c0mpute plugin install secure-chat`.

## Open questions

- **Offline storage quota.** How many bytes per recipient DID should a relay
  node store? Proposing 10 MB default, configurable by node operator.
- **Key revocation propagation.** When a user rotates their key, how quickly do
  all DHT nodes see the revocation? We need a `created_at` ordering + max-TTL
  rule. Needs design.
- **DID resolution speed.** DHT lookups add latency to first message. Should we
  cache resolved pubkeys locally with a short TTL (e.g., 1 hour)?
- **Password reset / key recovery.** If a user loses both the backup file and
  their password, they lose access to historical messages. Is there a social
  recovery option (e.g., threshold signature from trusted contacts)? Probably
  out of scope for v1 but worth flagging.
- **Spam at scale.** Rate limiting by DID is effective for low-volume abuse but
  a Sybil attacker can create many DIDs cheaply. Does CoinPay DID creation have
  a cost (e.g., stake requirement) that naturally limits this? Worth confirming
  with the CoinPay team.
- **Notification hooks.** Should `c0mpute chat` be able to trigger a shell hook
  (like `on-message` in Claude hooks) so users can wire desktop notifications?

## Out of scope

- Voice / video calls
- File transfer (use the storage plugin + send a link via chat)
- Read receipts (leaks online status; deliberately excluded)
- Message editing or deletion (cryptographically tricky with distributed relay)
- Cross-network federation with external chat systems (Matrix, XMPP, etc.)
- Double Ratchet / forward secrecy (v2)
- Onion routing / full traffic analysis resistance (v2+)
