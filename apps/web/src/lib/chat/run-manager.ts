// The one RunManager of this server process, shared by the route handlers and
// by instrumentation (which starts it at boot so dead servers' runs are taken
// over without waiting for a request). Kept on globalThis because Next can load
// this module more than once.

import { convoKey } from "./convo-store";
import { pluginKey } from "./plugin-store";
import { chatRunOpener } from "./run-opener";
import { createRunManager, type RunManager } from "./runs";
import { routerEnabled } from "./routed";
import { cloudSql, env, infernetFor, modelRouter } from "./server";

const g = globalThis as unknown as { __c0RunManager?: RunManager };

export function runManager(): RunManager {
  if (g.__c0RunManager) return g.__c0RunManager;
  // Sealed with the conversation key (C0MPUTE_CHAT_KEY or derived from the
  // plugin key); a run request holds the prompts.
  const key = convoKey(pluginKey(env("C0MPUTE_CHAT_KEY")), pluginKey(env("C0MPUTE_PLUGIN_KEY")));
  const durable = !!key && !!env("SQLITE_CLOUD_URL");
  g.__c0RunManager = createRunManager({
    opener: chatRunOpener((subject) => infernetFor(subject), { router: () => (routerEnabled(env) ? modelRouter() : null) }),
    sql: durable ? cloudSql : null,
    key,
  });
  return g.__c0RunManager;
}
