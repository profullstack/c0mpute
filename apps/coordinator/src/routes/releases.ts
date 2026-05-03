import { Hono } from "hono";

export const releases = new Hono();

/**
 * Release manifest fetched by `quest upgrade` and on-startup poll. Today this
 * is a static stub; once we have a CI pipeline cutting tagged releases this
 * reads from a `releases` table or a static JSON in object storage.
 */
releases.get("/latest", (c) =>
  c.json({
    version: "0.1.0",
    channel: "stable",
    min_required: "0.1.0",
    blocked_rollback: [],
    artifacts: [
      {
        os: "linux",
        arch: "x86_64",
        url: "https://depin.quest/video/releases/0.1.0/quest-linux-x86_64.tar.gz",
        sha256_hex: "",
        minisig_url:
          "https://depin.quest/video/releases/0.1.0/quest-linux-x86_64.tar.gz.minisig",
      },
    ],
  }),
);
