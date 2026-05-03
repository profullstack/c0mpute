/**
 * Quest coordinator API. Mounted under {BASE_PATH}/api/v1 (default /video).
 *
 * The coordinator owns: video lifecycle, job dispatch, provider registry,
 * verification challenges, billing webhooks. Postgres is the source of truth
 * (via Supabase); this process is stateless and horizontally scalable.
 */

import { Hono } from "hono";
import { logger } from "hono/logger";
import { cors } from "hono/cors";

import { videos } from "./routes/videos.ts";
import { jobs } from "./routes/jobs.ts";
import { providers } from "./routes/providers.ts";
import { earnings } from "./routes/earnings.ts";
import { billing } from "./routes/billing.ts";
import { webhooks } from "./routes/webhooks.ts";
import { network, knownIssues } from "./routes/network.ts";
import { releases } from "./routes/releases.ts";

const BASE_PATH = process.env.BASE_PATH ?? "/video";
const PORT = Number(process.env.PORT ?? 8787);

const app = new Hono();
app.use("*", logger());
app.use("*", cors());

const v1 = new Hono();
v1.route("/videos", videos);
v1.route("/jobs", jobs);
v1.route("/providers", providers);
v1.route("/providers", earnings); // /providers/:id/earnings lives here
v1.route("/billing", billing);
v1.route("/webhooks", webhooks);
v1.route("/network", network);
v1.route("/releases", releases);
v1.route("/known-issues", knownIssues);

app.route(`${BASE_PATH}/api/v1`, v1);

app.get("/health", (c) => c.json({ ok: true, basePath: BASE_PATH }));
app.get(`${BASE_PATH}/health`, (c) =>
  c.json({ ok: true, basePath: BASE_PATH }),
);

const server = Bun.serve({ port: PORT, fetch: app.fetch });
console.log(
  `quest-coordinator listening on :${server.port} (base ${BASE_PATH}/api/v1)`,
);
