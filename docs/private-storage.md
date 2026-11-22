# Private storage: node operators only ever hold ciphertext

Anything you store on the c0mpute network that is not deliberately public is
encrypted on your own machine before it leaves, with keys only you hold. The
nodes that store, replicate, serve and prove your data hold opaque bytes and
no key. This is the `private` tier of [DIP-0012](../dips/0012-storage-plugin.md),
and it is the default.

Public hosted websites (`c0mpute hosting publish`) stay plaintext: they are
public by design. Everything else is private unless you say `--public`.

## Use it

```sh
# First use creates your c0mpute identity and makes you save its encrypted
# backup (a file on another disk, or confirm you copied the printed text).
c0mpute storage key init --backup-to /media/usb/c0mpute-identity.json

# Store a file: encrypted here, published as ciphertext, 3 self-healing copies.
c0mpute storage put taxes-2026.pdf
#   c0p1:3f0c9a1e5b7d2c4a8e6f

# Read it back on any machine with your identity (dm restore puts it there).
c0mpute storage get c0p1:3f0c9a1e5b7d2c4a8e6f -o taxes-2026.pdf

# Share with someone: they run `c0mpute storage key show` and send you their
# c0ps1... identity. Only the key header changes; nothing is re-uploaded.
c0mpute storage share c0p1:3f0c9a1e5b7d2c4a8e6f --with c0ps1.Zb3...

# Stop sharing: re-encrypts under a new key and publishes the new version.
c0mpute storage revoke c0p1:3f0c9a1e5b7d2c4a8e6f --from c0ps1.Zb3...

# A link anyone can open (the key is after #, which browsers never send):
c0mpute storage link c0p1:3f0c9a1e5b7d2c4a8e6f
#   https://gateway.c0mpute.com/p/3f0c9a1e5b7d2c4a8e6f/#k=...

# Your private objects (from an encrypted index on this machine):
c0mpute storage ls --private

# Delete: publishes a tombstone, holders drop the ciphertext.
c0mpute storage rm c0p1:3f0c9a1e5b7d2c4a8e6f --yes

# Plaintext, on purpose (readable by anyone with the hash):
c0mpute storage put press-kit.zip --public
```

### Private sites

```sh
c0mpute hosting publish ./family-photos --name family-photos --private
#   open: https://gateway.c0mpute.com/p/<id>/#k=<key>
```

The gateway serves a fixed loader page and the site's ciphertext; the browser
decrypts it with the key from the link, checks the owner's signature (it needs
a browser with Ed25519 in WebCrypto: Chrome 137, Firefox 129, Safari 17 or
later), and renders it in a sandboxed frame with an opaque origin: it cannot
read the gateway's origin, open popups, or load anything from other hosts
(it can still make requests to the gateway itself). Republishing the
same `--name` keeps the link working; `--rekey` kills every link given out.
Relative links, images, stylesheets and scripts inside the site work; code
that `fetch()`es other files of the site does not.

### Encrypted volume (Linux)

```sh
c0mpute storage mount ~/Private --volume backups
```

A FUSE filesystem: apps and backup tools read and write ordinary files.
Each file is sealed when it is closed and stored as its own private object;
the directory tree is one more private object. Whole-file semantics (a file
is fetched into memory when opened), so it suits documents, photos and backup
archives, not databases. Needs `fusermount3` (package `fuse3`).

## Publish from apps

An app (PairUX, or anything else) can put a file into **your** private
storage without the app, its servers or c0mpute.com ever holding a key that
opens it. The app encrypts in your browser or phone, to your own storage
identity; you read the result with your own CLI.

```sh
# Once, on the computer that holds your c0mpute identity:
c0mpute storage key link            # signs in to c0mpute.com (CoinPay) if needed
c0mpute storage key link --status
#   linked:      c0ps1.Zb3...
c0mpute storage key unlink          # apps can no longer upload for you
```

Then connect the app (it asks c0mpute.com for the `storage:write` scope; the
consent screen says it can upload encrypted files into your private storage
and cannot read them), and anything it publishes is yours:

```sh
c0mpute storage get c0p1:3f0c9a1e5b7d2c4a8e6f -o recording.webm
c0mpute storage link c0p1:3f0c9a1e5b7d2c4a8e6f      # a #k= link, as usual
c0mpute storage share|revoke|rm c0p1:3f0c9a1e5b7d2c4a8e6f
```

Your linked identity and the uploads apps made appear on c0mpute.com (chat,
"CLI / API / MCP keys", private storage), with unlink.

### How it works

1. **Link.** `storage key link` proves this machine holds the X25519 storage
   key behind your `c0ps1` identity: c0mpute.com sends a fresh ephemeral
   X25519 key and a nonce, and the CLI answers with
   `HKDF-SHA256(X25519(storage key, server key), salt = server key || your
   key, "c0mpute-storage-link-v1\n" challenge "\n" nonce "\n" identity)`.
   An X25519 key cannot sign; this is the standard proof of possession for a
   Diffie-Hellman key, and it proves exactly the key apps will encrypt to.
   c0mpute.com stores the public identity on your account, bound to your
   CoinPay `sub` like everything else there.
2. **Authorize.** The app gets an OAuth 2.1 token from c0mpute.com with the
   `storage:write` scope (a `c0k_` key that works only on the storage upload
   routes; you can revoke it with your other keys).
3. **Seal.** In your browser, the app fetches your public identity and seals
   the file with `@profullstack/c0mpute-sealed`: the same `c0mpute-private-v1`
   format the CLI writes (random data key, HKDF-SHA256, AES-256-GCM STREAM in
   64 KiB chunks, Padmé padding), with the data key wrapped **only** to your
   identity. Files over 192 MiB become parts plus a root, as with the CLI.
4. **Upload.** c0mpute.com checks your quota and signs a short-lived upload
   ticket; the app streams the ciphertext straight to a gateway that takes
   uploads (`PUT /storage/v1/sealed/<site>?offset=N`, 16 MiB pieces,
   resumable) and asks it to publish. The gateway checks the ticket, the
   hosting record's signature, the sealed header's shape and the size, then
   holds the site and gossips it exactly as a publisher's node would: three
   self-healing copies, any gateway serves it.

### Ownership: who can change an app-made object

Objects you seal yourself are signed by a per-object key derived from your
identity seed, so your CLI can re-derive it to update, share, revoke or
delete. A browser never has your seed, so an app signs each object with a
**fresh** ed25519 key and puts that key's seed in the object's (encrypted)
metadata, wrapped to your storage key (`custody`: X25519 + HKDF + AES-256-GCM,
the key-slot construction under its own label, bound to the object key). Only
you can unwrap it, so `share`, `revoke`, `rm` and `link` work on app-made
objects as on your own (`revoke` keeps the object's key and address).

Trade-offs:

- **The app could have kept the per-object signing key** (it made it). With it
  the app can publish a new version of that one object or a tombstone, i.e.
  replace or delete what it uploaded, but never read it, and never touch
  anything else of yours. If that matters, re-store the file under your own
  key: `c0mpute storage get c0p1:... -o f && c0mpute storage put f`.
- **The owner-only roster** (who an object is shared with) needs a key only
  your seed derives, so an app fills it with random bytes; your CLI treats an
  object with custody as shared with you alone until you first `share` it.
- **c0mpute.com sees upload metadata:** which app uploaded how many bytes for
  your account, when, and the object ids (for quotas, the status endpoint and
  your account page). Never content, file names or keys: those are inside the
  ciphertext.
- **Sizes and the browser.** Sealing reads the file twice (hash, then
  encrypt) and holds a few MiB at a time, so a 500 MB video never sits in
  memory whole. Encryption runs on WebCrypto; on React Native add a WebCrypto
  polyfill.

### For app developers

```js
import { uploadSealed, status, linkStatus } from "@profullstack/c0mpute-sealed";

const who = await linkStatus({ token });           // { linked, identity, fingerprint }
const r = await uploadSealed(file, {                // File/Blob (fs.openAsBlob in Node)
  token,                                            // OAuth storage:write token
  onProgress: (p) => console.log(p.phase, p.done, p.total),
});
// r.ref = "c0p1:...": the owner runs `c0mpute storage get <ref>`
// r.link = https://gateway.c0mpute.com/p/<id>/#k=...: opens it; show, never store
await status(r.ref, { token });                    // { status, copies, target }
```

Getting a token (OAuth 2.1, public clients, PKCE S256, no client secret):

1. Register once: `POST https://c0mpute.com/api/oauth/register`
   `{"client_name":"PairUX","redirect_uris":["https://pairux.com/c0mpute/callback"]}`
   gives a `client_id`. Redirect URIs: https anywhere, or http on loopback
   (any port) for desktop/CLI apps; no fragments.
2. Send the person to
   `https://c0mpute.com/oauth/authorize?response_type=code&client_id=...&redirect_uri=...&scope=storage:write&state=...&code_challenge=<S256>&code_challenge_method=S256`.
   They sign in with CoinPay and approve.
3. `POST https://c0mpute.com/api/oauth/token` (form or JSON)
   `grant_type=authorization_code&code=...&client_id=...&redirect_uri=...&code_verifier=...`
   returns `{"access_token":"c0k_...","token_type":"Bearer","scope":"storage:write"}`.

API (all `Authorization: Bearer <token>`, CORS open):

| | |
| --- | --- |
| `GET /api/v1/storage/identity` | `{linked, identity: "c0ps1...", x25519, fingerprint, ingest_url}`; 409 `not_linked` |
| `POST /api/v1/storage/uploads` `{bytes, objects?}` | 201 `{upload_id, ticket, ingest_url, identity, max_bytes, max_objects, expires_at}` |
| `PUT <ingest_url>/storage/v1/sealed/<site_id>?offset=N` | body: ciphertext bytes (at most 16 MiB), `Authorization: Bearer <ticket>`; `{received}`; 409 with `received` to resume |
| `POST <ingest_url>/storage/v1/sealed/<site_id>/finish` `{record}` | the object's signed v1 hosting record; publishes; `{status, copies, target}` |
| `GET <ingest_url>/storage/v1/sealed/<site_id>` | `{published, copies, target}` (public); with the ticket also `{received, finished}` |
| `POST /api/v1/storage/uploads/<id>` `{ref, site_ids}` | record what was published; returns replication status |
| `GET /api/v1/storage/uploads/<id>`, `GET /api/v1/storage/objects/<ref>` | `{status: publishing\|replicating\|replicated, copies, target, objects}` |
| `GET /api/v1/storage/config` | public: ingest URL, ticket issuer key, limits |

Limits (per account, c0mpute.com settings): one upload (one file, all parts)
up to 4 GiB of ciphertext, 20 GiB per rolling 24 hours, 200 GiB through apps
in all, 60 uploads per hour; one object up to 200 MiB (a 192 MiB part); 16
MiB per PUT; a ticket lasts 6 hours.

Gateway operators take app uploads by naming c0mpute.com's ticket key
(`ticket_issuer` from `/api/v1/storage/config`):

```toml
[hosting]
sealed_ingest_issuer = "<64 hex>"    # or C0MPUTE_SEALED_INGEST_ISSUER
```

## What an operator can and cannot see

| An operator holding your data | can see | cannot see |
|---|---|---|
| Content | ciphertext | any plaintext byte |
| File names, types, folder structure | nothing (encrypted inside the object) | |
| Size | a padded size: lengths round up to Padmé buckets (at most about 12% over, a few bits of the true length); everything under 4 KiB looks the same | the exact size |
| Owner | a per-object signing key and a random 26-character name, different for every object, announced by a throwaway peer id per session | who owns it from the object alone. Objects published in one session share a source address and arrive close together in time, and a volume rewrites its directory object after each file, so a node watching the gossip can guess that some objects belong together |
| Sharing | 16 fixed key slots per object, all shaped alike | who it is shared with, or with how many |
| Time | when an object is written, updated and read, and from where | why |
| That it is private | yes: a private object is a hosting site with one opaque file | |

Gateways never decrypt. A private link's key lives after `#`, which browsers do
not send to servers, and the CLI never sends a key or plaintext to any node,
gateway, buyer bridge or log.

What protects you is the key, not an access list: reads of the ciphertext are
open (any gateway serves `/s/<id>/o`), exactly like public objects.

### What we do not protect against

- **Lost keys mean lost data.** There is no recovery, by design. Keep the
  encrypted identity backup safe and remember its password.
- **A compromised machine.** Malware on the computer that holds your identity
  can read your data, as with any end-to-end scheme.
- **Revocation is forward-only.** Anyone you shared with may have kept a copy
  of what they could read. `revoke` re-encrypts, so old keys and links open
  nothing current.
- **Traffic analysis.** Access timing and padded sizes are visible.
- **Replay of an older version.** Every object carries a signed version
  number, and the CLI remembers the newest version it has seen of each object
  and refuses older ones, so a node cannot quietly undo a revocation or a
  volume write on a machine that saw the newer version. A machine that never
  saw it cannot tell, and the browser loader does not keep this memory.
- **The browser loader is served from the gateway's own origin**, the one
  public sites under `/s/` are served from too. With the default
  `[gateway] sandbox_shared_sites = true` those public pages are sandboxed
  into origins of their own and cannot script the loader's origin; an
  operator who turns that off puts them back on the loader's origin (see
  [hosting](hosting.md#site-origins)). The key stays in the fragment and
  the decrypted page is sandboxed either way, but a gateway you do not
  trust can serve a different loader page; for anything sensitive,
  `c0mpute storage get` is the stronger path.
- **Post-quantum.** Key wrapping is X25519. Data with a decades-long secrecy
  horizon should wait for a hybrid ML-KEM wrap (the primitives are already in
  the tree for `c0mpute dm`).

## How it works

The format is `c0mpute-private-v1`, implemented in the `c0mpute-sealed` crate.

- **Data key.** Every object gets a random 256-bit data key (DEK). It is never
  used directly: the body key and a key commitment are derived from it with
  HKDF-SHA256 and a random 128-bit stream id, so every encryption has its own
  body key and nonce space.
- **Content and metadata.** The plaintext stream is
  `metadata JSON | owner signature | content | zero padding`, cut into 64 KiB
  chunks and sealed with AES-256-GCM in the STREAM construction: the chunk
  index and a last-chunk flag are bound into both the nonce and the associated
  data, so reordering, dropping or appending chunks fails authentication.
- **Key commitment.** AES-GCM is not key-committing, so the header carries
  `HKDF(DEK, stream id, "commit")`. A reader checks it before decrypting, so one
  object cannot open to different plaintexts under different keys.
- **Key wrapping.** The DEK is wrapped to each reader's X25519 storage key
  (ephemeral X25519, HKDF, AES-256-GCM) in one of 16 fixed slots; unused slots
  hold a fresh public key and random bytes, so they look like real ones. Slots
  carry no recipient id: readers find theirs by trying them.
  The owner also keeps an encrypted roster of who the object is wrapped to,
  which other readers cannot open.
- **Authenticity.** The owner signs the content hash, metadata hash (which
  includes a monotonic version), stream id and commitment with a per-object
  ed25519 key. Readers (the CLI and the browser loader) verify it and check
  that the site id is derived from that key, so a node, a gateway or another
  key holder cannot substitute or forge an object, even one encrypted to the
  reader's key.
- **No convergent encryption.** The same file stored twice produces unrelated
  ciphertext, so nobody can confirm you hold a known file.
- **Keys.** Storage keys are derived from your c0mpute identity seed (the one
  `c0mpute dm` uses; Argon2id-protected keyfile, encrypted backup) under their
  own HKDF labels. Restoring that backup restores access to everything.
- **On the network.** The sealed blob is published as a hosting site with one
  file, so replication (3 copies), healing, gateway serving and
  proof-of-storage challenges all run unchanged over ciphertext. Files over
  192 MiB are split into separately sealed parts whose keys live only inside
  the sealed root object.

All key types zeroize on drop.
