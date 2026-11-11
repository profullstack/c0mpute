# c0mpute in ChatGPT, Claude and Claude Code

c0mpute.com runs a remote MCP server at **`https://c0mpute.com/api/mcp`**. Add
it to an assistant and you can say "put this page on the network" and get back
a gateway URL, the same thing `c0mpute hosting publish` does from a terminal.

| Client | How |
| --- | --- |
| Claude (claude.ai, desktop, mobile) | Settings, Connectors, Add custom connector, URL `https://c0mpute.com/api/mcp` |
| ChatGPT | Settings, Apps & Connectors, Advanced, turn on Developer mode, then Create: URL `https://c0mpute.com/api/mcp`, authentication OAuth |
| Claude Code | `/plugin marketplace add profullstack/claude-plugins` then `/plugin install c0mpute@profullstack`, or `claude mcp add --transport http c0mpute https://c0mpute.com/api/mcp` |
| Anything else | Any MCP client that speaks Streamable HTTP + OAuth 2.1, or send `Authorization: Bearer c0k_...` with a key from c0mpute chat |

## Signing in

The first tool call answers 401 with
`WWW-Authenticate: Bearer resource_metadata=".../.well-known/oauth-protected-resource"`.
The client then:

1. reads `/.well-known/oauth-protected-resource` and
   `/.well-known/oauth-authorization-server` (RFC 9728, RFC 8414);
2. registers itself at `POST /api/oauth/register` (RFC 7591, public client, no secret);
3. opens `/oauth/authorize` (PKCE S256). A signed-out person goes through
   "Sign in with CoinPay" (a CoinPay DID is required) and comes back to a
   consent page that names the app and where it returns to;
4. trades the code at `POST /api/oauth/token` for an access token.

The access token is an ordinary c0mpute API key labelled `<app> (OAuth)`. It
does not expire. Revoke it with your other keys in c0mpute chat.

Tables: `oauth_clients`, `oauth_codes` (hashed, single use, 5 minutes), in the
same SQLite Cloud database as `chat_tokens`. Code: `apps/web/src/lib/chat/oauth.ts`.

## Publishing without a node: the cloud node

`hosting_push` needs a node to sign and publish the site. Someone using the
ChatGPT or Claude connector usually has none, so the site falls back to the
**c0mpute cloud node**, one house node named by `C0MPUTE_CLOUD_NODE_ID` (the
`chat_nodes.id` of a node linked with `c0mpute chat connect hosting`).

- It publishes with `--disks free` (the network's own copies), so it never spends a wallet.
- Inline text files only (HTML, CSS, JS, SVG, Markdown, up to 1 MiB per push). Media refs are fetched with the publishing node's key, which cannot read another account's chat results.
- It signs every site with one key, so site names are first come, first served across accounts.
- At most 5 sites per account. A person with their own linked node keeps using it; `node: "cloud"` picks the cloud node on purpose.
- Unset `C0MPUTE_CLOUD_NODE_ID` and nothing changes: pushes need your own node again.

Run the cloud node like any other push node, on a box with disk (not dev2):

```sh
c0mpute chat login                    # as the house account
c0mpute chat connect hosting --name cloud
```

then put its node id in `C0MPUTE_CLOUD_NODE_ID` in the site's app.env and the
`c0mpute--prod` vault.
