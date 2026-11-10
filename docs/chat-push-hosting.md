# `/push hosting`: publish what a chat made

In [c0mpute.com/chat](https://c0mpute.com/chat), everything a conversation
produced (images, songs and their covers, album zips, promo videos, and the
transcript as `index.html` + `chat.md`) can be published as a c0mpute hosting
site with paid hosts from the d1sks marketplace: the same thing as

```sh
c0mpute hosting publish ./done --name seed2 --disks auto
```

but from the chat.

## Commands

| in the chat | does |
| --- | --- |
| `/connect hosting` | list the hosting nodes linked to your account, and how to link one |
| `/connect hosting <node>` | publish through that node from now on (remembered in this browser) |
| `/push hosting <name>` | publish this chat's files as site `<name>` (again = a new version) |
| `/push hosting ls` | your sites: status, node, url |
| `/push hosting rm <name>` | take a site off the network |

`/push <name>` is short for `/push hosting <name>`. A push shows a live card:
spinner, elapsed time, and the node's latest progress line (downloading,
hashing, picking hosts, copies), then the site url.

## Linking a node

c0mpute.com never publishes anything itself. On the machine that should
publish (it needs to stay up while the network copies the site):

```sh
c0mpute chat login                          # CoinPay sign-in, same account as the web chat
c0mpute chat connect hosting --name seed2   # link + serve pushes; leave it running
```

The node signs every site with its own key (so only that node can update or
remove it) and pays its hosts from its own CoinPay wallet. Pushes are refused
(HTTP 402) until the node has CoinPay payments on:

```toml
# ~/.config/c0mpute/config.toml
[payments]
enabled = true
rail = "coinpay"
[payments.coinpay]
api_key_env = "COINPAY_API_KEY"   # the key itself goes in the environment
recipient_email = "you@example.com"
```

Options: `--interval <s>` (poll, default 5), `--once`, `--wait-secs <s>`
(passed to `hosting publish`), `--dir <path>` (default `<data_dir>/pushes`).

## Accounts and ownership

Nodes (`chat_nodes`) and pushes (`chat_pushes`) are rows under the signed-in
CoinPay account in the site database. Every API call is scoped to that
account: nobody can see, update or delete another account's pushes, and the
same site name under two accounts is two different sites (different node keys
give different site ids).

## API

All take the chat session cookie or `Authorization: Bearer c0k_...`.

| | |
| --- | --- |
| `GET /api/v1/hosting/nodes` | your nodes (`online` = polled in the last 2 min) |
| `POST /api/v1/hosting/nodes` | a node links itself (API key only) |
| `DELETE /api/v1/hosting/nodes?node=<name>` | unlink (its sites stay up) |
| `GET /api/v1/hosting/nodes/<id>/jobs` | the node's poll: claims queued pushes and deletes |
| `GET /api/v1/hosting/pushes` | your pushes |
| `POST /api/v1/hosting/pushes` `{name, node?, files}` | queue a push (202) |
| `GET /api/v1/hosting/pushes/<id or name>` | status, `progress`, `url` |
| `DELETE /api/v1/hosting/pushes/<id or name>` | remove (202 while the node takes it down) |
| `POST /api/v1/hosting/pushes/<id>` | the node reports progress or the result |
| `GET /api/v1/hosting/pushes/<id>/files/<n>` | one file, for the node |

Media files are references to the chat's own result routes
(`/api/v1/images/<id>`, `/api/v1/music/<id>`, `/api/v1/album/<id>/zip`,
`/api/v1/promo/<id>/video`, ...), streamed to the node on demand, so nothing
large is stored on c0mpute.com. Results that have expired from the network's
media store are left out of the site and listed on the push.
