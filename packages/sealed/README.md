# @profullstack/c0mpute-sealed

Publish files into a person's [c0mpute](https://c0mpute.com) private storage
from your app (browser, React Native, Node 20+), end-to-end encrypted. The
file is sealed on the person's device to their own storage identity, so
neither your servers nor c0mpute.com ever hold a key that opens it. They read
it with `c0mpute storage get c0p1:...`.

```sh
npm install @profullstack/c0mpute-sealed
```

## Use

```js
import { uploadSealed, linkStatus, status } from "@profullstack/c0mpute-sealed";

// token: a c0mpute.com OAuth 2.1 access token with the storage:write scope
const who = await linkStatus({ token });
if (!who.linked) {
  // Ask the person to run `c0mpute storage key link` once.
}

const r = await uploadSealed(file, {           // File / Blob / Uint8Array
  token,
  onProgress: ({ phase, done, total }) => render(phase, done / total),
});
r.ref;   // "c0p1:3f0c9a1e5b7d2c4a8e6f": the person runs `c0mpute storage get <ref>`
r.link;  // https://gateway.c0mpute.com/p/<id>/#k=...: opens it in a browser.
         // Treat r.key and r.link like passwords: show them, never store or send them.

await status(r.ref, { token });               // { status: "replicating", copies: 2, target: 3 }
```

Node: `uploadSealed(await fs.openAsBlob("video.mp4"), { token })`. A 500 MB
file is read twice (hash, then encrypt) in 4 MiB slices and sent in 8 MiB
pieces; it never sits in memory whole. Files over 192 MiB become parts plus a
root object, like the CLI does.

## Getting a token (OAuth 2.1 + PKCE, public client)

1. Register once (no secret is issued):

   ```sh
   curl -X POST https://c0mpute.com/api/oauth/register \
     -H 'content-type: application/json' \
     -d '{"client_name":"Your App","redirect_uris":["https://yourapp.example/c0mpute/callback"]}'
   # {"client_id":"c0c_...", ...}
   ```

   Redirect URIs must be https, or http on localhost/127.0.0.1/[::1] (any
   port) for desktop and CLI apps. No fragments.

2. Send the person to

   ```
   https://c0mpute.com/oauth/authorize?response_type=code&client_id=c0c_...
     &redirect_uri=https://yourapp.example/c0mpute/callback&scope=storage:write
     &state=<random>&code_challenge=<BASE64URL(SHA256(verifier))>&code_challenge_method=S256
   ```

   They sign in with CoinPay and see: "upload encrypted files into your
   private storage. It cannot read them".

3. Exchange the code:

   ```sh
   curl -X POST https://c0mpute.com/api/oauth/token \
     -d grant_type=authorization_code -d code=c0a_... -d client_id=c0c_... \
     -d redirect_uri=https://yourapp.example/c0mpute/callback -d code_verifier=<verifier>
   # {"access_token":"c0k_...","token_type":"Bearer","scope":"storage:write"}
   ```

The token works only on the storage upload routes; the person can revoke it
on c0mpute.com.

## API

| function | |
| --- | --- |
| `identity({ token })` | the linked identity `{ identity: "c0ps1...", x25519, fingerprint, ingest_url }`; throws `code: "not_linked"` (409) |
| `linkStatus({ token })` | same, or `{ linked: false }` |
| `uploadSealed(file, { token, fileName?, contentType?, onProgress?, signal? })` | `{ ref, siteId, key, link, uploadId, status, copies, bytes }` |
| `status(ref, { token })` | `{ status: "publishing" \| "replicating" \| "replicated", copies, target, objects }` |
| `link(ref, key, { gateway? })` | `https://gateway.c0mpute.com/p/<id>/#k=<key>` |

Low level, for your own transport: `planSealed(file, { identity })`,
`streamObject(plannedObject, write)` (returns the signed hosting record),
`openSealed(blob, key)`, `fingerprint(identity)`.

All calls take `baseUrl` (default `https://c0mpute.com`) and `fetch`.

## What is protected

- The data key is drawn on the device and wrapped only to the person's X25519
  storage key. Content, file name and type are inside the ciphertext.
- Each object is signed by a fresh per-object ed25519 key; its seed is wrapped
  to the person ("custody"), so their CLI can share, revoke and delete it.
  Your app made that key, so it could also replace or delete that one object
  (never read it). See
  [docs/private-storage.md](https://github.com/profullstack/c0mpute/blob/master/docs/private-storage.md#publish-from-apps).
- c0mpute.com sees which app uploaded how many bytes, when, and the object
  ids; never keys or plaintext.

The format is `c0mpute-private-v1`, the same bytes the Rust CLI writes; CI
opens objects made by this library with the CLI's code.

## Runtimes

WebCrypto AES-GCM (browsers, Node 20+, Deno, Bun) plus @noble curves and
hashes. React Native: install a WebCrypto polyfill (for example
`react-native-quick-crypto`) and `react-native-get-random-values`.

MIT
