# Gallery API

`GET https://c0mpute.com/api/v1/gallery` lists things built with, or hosted on,
the c0mpute network. It is the single source of truth for
[c0mpute.com/gallery](https://c0mpute.com/gallery) and is read by
infernetprotocol.com/gallery.

- Public, no key. CORS open (`Access-Control-Allow-Origin: *`, `OPTIONS` answered).
- Cached: `Cache-Control: public, max-age=60, s-maxage=300`. Listings change without a deploy (submissions, badge checks).
- Source: the `gallery_listings` table in the site database (`apps/web/src/lib/gallery/store.ts`), seeded once from the editorial list in `apps/web/src/lib/gallery.ts` (served as-is if the database is unreachable). Seed screenshots: `apps/web/public/gallery/<slug>.png`; submitted listings get a generated card at `/gallery/card/<slug>`.

## Response

```json
{
  "object": "list",
  "schema_version": 1,
  "updated_at": "2026-11-22",
  "count": 8,
  "categories": [
    { "id": "apps", "label": "apps", "count": 2 }
  ],
  "submit_url": "https://c0mpute.com/gallery/submit",
  "data": [
    {
      "slug": "board-saas",
      "name": "Board as a Service",
      "tagline": "Paste a decision brief: five AI advisors and a chair ...",
      "url": "https://board.saas.pit.world",
      "c0mpute_url": "https://gateway.c0mpute.com/s/d28960255346f386e99c/",
      "category": "apps",
      "built_with": ["c0mpute chat API", "c0mpute hosting"],
      "image": "https://c0mpute.com/gallery/board-saas.png",
      "added_at": "2026-11-22",
      "featured": false,
      "listed_via": "editorial"
    }
  ]
}
```

| field | type | meaning |
|---|---|---|
| `slug` | string | Stable id, lowercase kebab-case. Never changes once published; safe as a key or anchor. |
| `name` | string | Display name. |
| `tagline` | string | One or two sentences, plain text. |
| `url` | string | The live site, model page or app. Answered 200 when the entry was added. |
| `c0mpute_url` | string \| null | The copy served by c0mpute hosting (a `gateway.c0mpute.com/s/<site_id>/` URL), or `null` when the thing runs on c0mpute but is not hosted there. |
| `category` | string | One of the `categories[].id` values: `apps`, `automation`, `media`, `models`. New ids may appear; show unknown ones as-is. |
| `built_with` | string[] | c0mpute features it uses: `c0mpute hosting`, `c0mpute chat API`, `Infernet chat`, `c0mpute jobs`, `c0mpute music`, `c0mpute speech`, `fine-tune`, `model router`. Open vocabulary. |
| `image` | string | Absolute URL of a 1280x800 PNG (16:10): a screenshot, or a generated card for submitted listings. |
| `added_at` | string | `YYYY-MM-DD`, when it went live on c0mpute. |
| `featured` | boolean | Picked out by the editors; c0mpute.com/gallery shows these first. Added 2026-11-22. |
| `listed_via` | string | How it got listed: `editorial`, `badge` (free tier, badge found on the site), `queue` (free tier, 30-day queue), `priority` (paid review) or `admin`. Added 2026-11-22. |

`data` is ordered newest first by `added_at` (ties in editorial order). `submit_url` is the "Get listed" page.

## Stability

`schema_version` is 1. Fields may be added at any time. A field is never
renamed, retyped or removed without bumping `schema_version`, so a reader that
ignores unknown fields keeps working.

## Get listed (submissions)

Anyone can submit at [c0mpute.com/gallery/submit](https://c0mpute.com/gallery/submit),
from the CLI (`c0mpute gallery submit|status|list`), over MCP (`gallery_submit`,
`gallery_status`, `gallery_list` on `https://c0mpute.com/api/mcp`) or the API below.
Listings are keyed by URL only (normalized: no scheme, no `www.`, no trailing slash,
query or fragment), so one site is one listing.

- **Free:** listed as soon as the badge is found on the submitted page or its homepage;
  without it, after a 30-day queue. Badge listings are re-checked weekly; a badge missing
  on two checks a day apart moves the listing back to the queue (the contact is emailed).
- **Priority:** a one-time CoinPay payment (price in `apps/web/src/lib/gallery/config.ts`,
  `priorityPriceUsd`); live as soon as it confirms and the automated checks pass.
- **Automated checks:** the URL answers 200 (fetched server-side with SSRF guards), the
  domain is clear on NicheDB reputation (Spamhaus DBL, URIBL, SURBL), and neither the
  listing text nor the page title/description trips the content policy or carries an
  adult rating label (those wait for a person).

| method + path | what |
|---|---|
| `POST /api/v1/gallery/submissions` | Submit. JSON or form: `url` (required), `name`, `tagline`, `category`, `built_with` (array or comma list), `contact_email` (required for priority), `tier` (`free` \| `priority`). 201 `{listing, checkout_url, message}`; 409 already submitted (body has its `listing`); 422 not listable (not 200, blocklisted). 10 per IP per hour. |
| `GET /api/v1/gallery/submissions?url=<site>` | A listing's public status by URL. |
| `GET /api/v1/gallery/submissions/<id>` | Same, by id (`gl_...`) or slug. |
| `POST /api/v1/gallery/submissions/<id>/check` | "Check my badge now" (once a minute per listing). |
| `POST /api/v1/gallery/submissions/<id>/checkout` | Open a CoinPay checkout for priority (also upgrades a queued free listing). `{checkout_url}`; a form post 303s to it. |
| `POST /api/v1/gallery/webhook` | CoinPay events (signed `X-CoinPay-Signature`; unsigned calls get 401). |
| `GET /api/v1/gallery/admin?status=` / `POST /api/v1/gallery/admin/<id>` | Moderation (`approve`, `reject` with `reason` + `refund`, `feature`, `unfeature`, `requeue`, `delete`). Moderators only: `C0MPUTE_ADMIN_SUBS` or Infernet admins. UI at `/gallery/admin`. |

A listing (`object: "gallery.listing"`) has `id`, `slug`, `url`, `name`, `tagline`,
`category`, `built_with`, `tier`, `status` (`queued`, `awaiting_payment`,
`priority_review`, `review`, `approved`, `rejected`), `listed_via`, `featured`,
`goes_live_at` (queued only), `added_at` (approved only), `paid`, `badge` (`state`:
`found` \| `missing` \| `error`, `checked_at`, `missing_since`, `hits`), `note`,
`status_url`, `gallery_url`, `badge_svg`, `snippet`, `markdown` and
`priority_price_usd`. The contact email is never shown.

## The badge

`GET https://c0mpute.com/badge.svg?url=<site url>[&theme=dark]`: 170x36 SVG, the same
image for every site, cached a day, CORS open, no script, no cookie. The only thing
recorded is a hit count per URL. Without `?url=` the embedding page's `Referer` is used.
A badge load from a site waiting in the free queue triggers a check of that site.

```html
<a href="https://c0mpute.com/gallery?ref=https%3A%2F%2Fexample.com"><img src="https://c0mpute.com/badge.svg?url=https%3A%2F%2Fexample.com" alt="Listed on the c0mpute gallery" width="170" height="36"></a>
```

The verifier looks for any `<a href>` or `<img src>` pointing at `c0mpute.com/badge.svg`
or `c0mpute.com/gallery` in the server-rendered HTML. Profullstack sites get it from
`@profullstack/footer` 0.7.0+ with `c0mputeBadge` on.

## Adding an editorial entry

Append it to `GALLERY` in `apps/web/src/lib/gallery.ts` only if it should be part of
the seed for a fresh database; on the live site, submit it (or approve it in
`/gallery/admin`). Editorial entries need a 1280x800 screenshot at
`apps/web/public/gallery/<slug>.png`. An entry goes in when it is live and really
uses the network; nothing is listed on claims alone, and no usage numbers are
published that we have not measured.
