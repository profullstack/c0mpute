# The Future of Hosting: landing page hosted on c0mpute

A static, dependency-free page (one `index.html`, inline SVG art, optional
network-generated PNGs in `img/`) that promotes c0mpute, d1sks and Infernet and
is itself served from the c0mpute hosting network.

Live: https://gateway.c0mpute.com/s/53021d6e8d5a0e1aac48/

## Publish it yourself

```sh
c0mpute hosting publish examples/landing-page --name future-of-hosting
# prints the site id; open https://gateway.c0mpute.com/s/<site_id>/
moshcode name c0mpute <label.ending> <site_id>   # optional Moshpit name
```

Publishing again with the same `--name` from the same machine updates the site
in place (same site id, new version).

## Art

The page ships with inline SVG cartoons. If `img/hero.png`, `img/disks.png` or
`img/brain.png` exist next to it, a small script swaps them in. Generate them on
the network with the CPU image workers (`POST /v1/images` on a buyer gateway,
see `c0mpute speech serve`), prompts in the commit history.

## Link previews (og:image, Twitter card)

`og.png` is a 1200x630 card rendered from `og-card.html`:

```sh
chromium --headless=new --hide-scrollbars --virtual-time-budget=4000 \
  --window-size=1200,630 --screenshot=og.png og-card.html
```

Crawlers ignore relative URLs, so `index.html` names the page and card by their
absolute gateway URLs (`og:url`, `og:image`, `twitter:image`, canonical). If you
publish your own copy, replace `53021d6e8d5a0e1aac48` with your site id. A small
script also adds the trailing slash to `/s/<site_id>` so relative assets resolve.
