// Money plugins (Mercury, CoinPay, moomoo): catalog entries, read-only
// allowlists and the confirmation gate. One source for c0mpute chat and
// Infernet chat: @profullstack/chat-plugins (src/money.js). This module only
// re-exports it so the rest of c0mpute imports from one local path.
//
// DOCS_CATALOG (Microsoft Learn) comes from the same package: public,
// read-only documentation servers with no sign-in, on by default, no policy.
//
// The gate runs in the tool-call layer (plugins.ts livePlugins/refreshTools):
// a tool on a money plugin runs alone only when its exact name is on that
// plugin's read-only allowlist; every other tool, including names never seen,
// is marked destructive before the loop sees it, so the turn stops for an
// approval card and only the signed approval for that exact call runs it.

export {
  MODEL_CAN_BE_WRONG,
  FINANCE_CATALOG,
  DOCS_CATALOG,
  DEFAULT_PLUGINS,
  mcpAddCommand,
  POLICIES,
  policyFor,
  needsConfirmation,
  visibleInMode,
  applyPolicy,
  describeAction,
  redactLog,
} from "@profullstack/chat-plugins";
export type { ActionSummary, DocsCatalogEntry, FinanceCatalogEntry, MoneyKind, PluginPolicy, TradingMode } from "@profullstack/chat-plugins";
