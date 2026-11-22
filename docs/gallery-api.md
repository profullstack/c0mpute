# Gallery API

`GET https://c0mpute.com/api/v1/gallery` lists things built with, or hosted on,
the c0mpute network. It is the single source of truth for
[c0mpute.com/gallery](https://c0mpute.com/gallery) and is read by
infernetprotocol.com/gallery.

- Public, no key. CORS open (`Access-Control-Allow-Origin: *`, `OPTIONS` answered).
- Cached: `Cache-Control: public, max-age=300, s-maxage=3600`. The list changes only on a c0mpute.com deploy.
- Source: `apps/web/src/lib/gallery.ts`. Screenshots: `apps/web/public/gallery/<slug>.png`.

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
  "submit_url": "https://github.com/profullstack/c0mpute/issues/new?labels=gallery&title=Gallery%3A+",
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
      "added_at": "2026-11-22"
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
| `image` | string | Absolute URL of a 1280x800 PNG screenshot (16:10). |
| `added_at` | string | `YYYY-MM-DD`, when it went live on c0mpute. |

`data` is ordered newest first by `added_at` (ties in editorial order).

## Stability

`schema_version` is 1. Fields may be added at any time. A field is never
renamed, retyped or removed without bumping `schema_version`, so a reader that
ignores unknown fields keeps working.

## Adding an entry

Open an issue with the `submit_url` (or email hello@c0mpute.com). An entry goes
in when it is live and really uses the network; nothing is listed on claims
alone, and no usage numbers are published that we have not measured. To add
one: append it to `GALLERY` in `apps/web/src/lib/gallery.ts`, save a 1280x800
screenshot as `apps/web/public/gallery/<slug>.png`, and deploy.
