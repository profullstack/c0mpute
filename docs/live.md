# c0mpute Live

Live streaming carried by the network's nodes (DIP-0019). You push RTMP; nodes
cut it into HLS, relay the segments to each other, and any gateway serves the
playlist to a browser, VLC, mpv or ffplay.

## Stream

1. Create a stream at <https://c0mpute.com/dashboard/live>, or with the API:

   ```sh
   curl -s https://c0mpute.com/api/v1/live/streams \
     -H "authorization: Bearer $C0MPUTE_KEY" -H 'content-type: application/json' \
     -d '{"name":"friday show"}'
   ```

   The reply has `stream_key` (shown once), `publish_url`, `watch_url` and
   `playback_url`.

2. Publish. In OBS or PairUX: server `rtmp://live.c0mpute.com/live`, key the
   stream key. With ffmpeg:

   ```sh
   ffmpeg -re -i show.mp4 -c:v libx264 -g 60 -c:a aac -f flv "$PUBLISH_URL"
   ```

3. Share `watch_url` (`https://c0mpute.com/live/<id>`). Players that take a
   playlist use `https://c0mpute.com/live/<id>/index.m3u8`.

`"visibility":"unlisted"` keeps a stream off <https://c0mpute.com/live>; anyone
with the link can still watch. The video is not encrypted on the network, so an
unlisted stream is private only as far as its link is.

## What it costs, and who is paid

- Relays are paid **$0.01 per GB** they carry, metered every 60 seconds. The
  operator keeps 95%; 5% is the house fee, and a node's referrer is paid out of
  that fee.
- The streamer pays from their c0mpute balance (the same one chat uses).
- With no balance the stream still runs on the free tier: up to 720p and 25
  viewers at once, and relays are not paid for it.
- One account is charged at most $25 an hour for relay; past that its streams
  drop to the free tier.

## Node operators

Every node relays live streams and is paid for it. There is nothing to turn
on. To opt out: `c0mpute worker start --no-live`, `C0MPUTE_LIVE=0`, or
`[live] enabled = false` in the config. Earnings show as "live stream relay".

## API

All under `https://c0mpute.com/api/v1/live`, with `Authorization: Bearer` an
API key or an OAuth token carrying the `live:publish` scope.

| Call | Does |
|---|---|
| `POST /streams {name, title?, visibility?}` | Create; returns the key once |
| `GET /streams` | List (never the keys) |
| `GET /streams/:id`, `DELETE /streams/:id` | Read; delete (the key stops working) |
| `POST /streams/:id/key` | A new key; the old one stops working |
| `GET /streams/:id/usage` | Bytes relayed and cost |
| `GET /me` | The account a connected app publishes as, balance and tier |

Apps connect with OAuth: the authorization server at
`https://c0mpute.com/.well-known/oauth-authorization-server` (public clients,
PKCE), scope `live:publish`. That scope can manage live streams and nothing
else on the account.

MCP tools: `live_stream_create`, `live_streams`, `live_stream_end`.
