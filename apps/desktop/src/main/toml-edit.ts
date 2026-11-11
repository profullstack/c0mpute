// Comment-preserving edits to ~/.config/c0mpute/config.toml.
//
// The node owner may have hand-written comments in config.toml, and a TOML
// round trip (parse + stringify) throws them away. So a form change edits one
// `key = value` line in place (or inserts it), and the result is parsed with
// smol-toml before anything is written, so a bad edit never reaches disk.

import { parse } from "smol-toml";

export type TomlScalar = string | number | boolean | string[];

export function tomlLiteral(v: TomlScalar): string {
  if (Array.isArray(v)) return `[${v.map((x) => tomlLiteral(x)).join(", ")}]`;
  if (typeof v === "boolean") return v ? "true" : "false";
  if (typeof v === "number") {
    if (!Number.isFinite(v)) throw new Error("not a finite number");
    return String(v);
  }
  return JSON.stringify(v); // a TOML basic string is a JSON string for printable text
}

const HEADER = /^\s*\[\s*([A-Za-z0-9_.\-"]+)\s*\]\s*(#.*)?$/;
const ARRAY_HEADER = /^\s*\[\[/;

function keyRe(key: string) {
  return new RegExp(`^\\s*${key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*=`);
}

/** Line range [start, end) of a section's body; root = before the first header. */
function sectionRange(lines: string[], section: string | null): [number, number] | null {
  if (section === null) {
    const first = lines.findIndex((l) => HEADER.test(l) || ARRAY_HEADER.test(l));
    return [0, first === -1 ? lines.length : first];
  }
  const start = lines.findIndex((l) => {
    const m = HEADER.exec(l);
    return m !== null && m[1] === section;
  });
  if (start === -1) return null;
  let end = start + 1;
  while (end < lines.length && !HEADER.test(lines[end]) && !ARRAY_HEADER.test(lines[end])) end++;
  return [start + 1, end];
}

/** Set `key = value` in `[section]` (null: the top level), keeping everything else. */
export function setTomlValue(text: string, section: string | null, key: string, value: TomlScalar): string {
  if (!/^[A-Za-z0-9_-]+$/.test(key)) throw new Error(`bad key ${key}`);
  const nl = text.includes("\r\n") ? "\r\n" : "\n";
  const lines = text.length ? text.split(/\r?\n/) : [];
  const line = `${key} = ${tomlLiteral(value)}`;
  const range = sectionRange(lines, section);
  if (!range) {
    while (lines.length && lines[lines.length - 1].trim() === "") lines.pop();
    if (lines.length) lines.push("");
    lines.push(`[${section}]`, line, "");
    return lines.join(nl);
  }
  const [a, b] = range;
  const re = keyRe(key);
  for (let i = a; i < b; i++) {
    if (re.test(lines[i])) {
      // A multi-line array value continues until its brackets balance.
      let j = i;
      if (/=\s*\[/.test(lines[i])) {
        let depth = 0;
        for (j = i; j < b; j++) {
          for (const c of lines[j].replace(/"(?:[^"\\]|\\.)*"/g, "")) {
            if (c === "[") depth++;
            if (c === "]") depth--;
          }
          if (depth <= 0) break;
        }
      }
      lines.splice(i, j - i + 1, line);
      return lines.join(nl);
    }
  }
  // Insert after the section's last non-blank line.
  let at = b;
  while (at > a && lines[at - 1].trim() === "") at--;
  lines.splice(at, 0, line);
  return lines.join(nl);
}

/** Remove `key` from `[section]` (so the CLI default applies). */
export function unsetTomlValue(text: string, section: string | null, key: string): string {
  const nl = text.includes("\r\n") ? "\r\n" : "\n";
  const lines = text.split(/\r?\n/);
  const range = sectionRange(lines, section);
  if (!range) return text;
  const re = keyRe(key);
  for (let i = range[0]; i < range[1]; i++) {
    if (re.test(lines[i])) {
      lines.splice(i, 1);
      break;
    }
  }
  return lines.join(nl);
}

export function isEnvVarName(s: string): boolean {
  return /^[A-Z_][A-Z0-9_]{0,127}$/.test(s);
}

/** Field names that would hold a secret; config.toml must never carry one. */
const SECRET_FIELDS = /^(api_key|apikey|key|secret|token|password|private_key)$/i;

export interface ConfigCheck {
  ok: boolean;
  error?: string;
  value?: Record<string, unknown>;
}

/**
 * Parses config.toml text and refuses a secret under [payments]: the rail's
 * key lives in the environment variable named by `api_key_env`.
 */
export function checkConfigText(text: string): ConfigCheck {
  let value: Record<string, unknown>;
  try {
    value = parse(text) as Record<string, unknown>;
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
  const payments = value.payments as Record<string, unknown> | undefined;
  const scan = (obj: Record<string, unknown> | undefined, where: string): string | undefined => {
    if (!obj || typeof obj !== "object") return undefined;
    for (const [k, v] of Object.entries(obj)) {
      if (SECRET_FIELDS.test(k)) return `${where}.${k}: put the key in an environment variable and name it in api_key_env, never in config.toml`;
      if (v && typeof v === "object" && !Array.isArray(v)) {
        const bad = scan(v as Record<string, unknown>, `${where}.${k}`);
        if (bad) return bad;
      }
    }
    return undefined;
  };
  const bad = scan(payments, "payments");
  if (bad) return { ok: false, error: bad };
  const env = (payments?.coinpay as Record<string, unknown> | undefined)?.api_key_env;
  if (env !== undefined && (typeof env !== "string" || !isEnvVarName(env))) {
    return { ok: false, error: "payments.coinpay.api_key_env must be an environment variable NAME (A-Z, 0-9, _), not the key itself" };
  }
  return { ok: true, value };
}

/** The settings the Node form edits, read from parsed config. */
export interface NodeSettings {
  storage: boolean;
  hosting: boolean;
  private: boolean;
  autoUpdate: boolean;
  gatewayBind: string;
  paymentsEnabled: boolean;
  paymentsRail: string;
  apiKeyEnv: string;
  recipientEmail: string;
  payoutWallet: string;
  payoutCoin: string;
}

export function readNodeSettings(cfg: Record<string, unknown>): NodeSettings {
  const sec = (k: string) => (cfg[k] as Record<string, unknown> | undefined) ?? {};
  const payments = sec("payments");
  const coinpay = (payments.coinpay as Record<string, unknown> | undefined) ?? {};
  const bool = (v: unknown, d: boolean) => (typeof v === "boolean" ? v : d);
  const str = (v: unknown, d: string) => (typeof v === "string" ? v : d);
  return {
    storage: bool(sec("storage").enabled, true),
    hosting: bool(sec("hosting").enabled, true),
    private: bool(sec("private").enabled, false),
    autoUpdate: bool(cfg.update_auto, true),
    gatewayBind: str(sec("gateway").bind, "0.0.0.0:7777"),
    paymentsEnabled: bool(payments.enabled, false),
    paymentsRail: str(payments.rail, "coinpay"),
    apiKeyEnv: str(coinpay.api_key_env, "C0MPUTE_COINPAY_PAYOUT_KEY"),
    recipientEmail: str(coinpay.recipient_email, ""),
    payoutWallet: str(sec("hosting").payout_wallet, ""),
    payoutCoin: str(sec("hosting").payout_coin, ""),
  };
}

/** Apply a form's changes to config text, one line at a time. */
export function applyNodeSettings(text: string, s: Partial<NodeSettings>): string {
  let t = text;
  if (s.storage !== undefined) t = setTomlValue(t, "storage", "enabled", s.storage);
  if (s.hosting !== undefined) t = setTomlValue(t, "hosting", "enabled", s.hosting);
  if (s.private !== undefined) t = setTomlValue(t, "private", "enabled", s.private);
  if (s.autoUpdate !== undefined) t = setTomlValue(t, null, "update_auto", s.autoUpdate);
  if (s.gatewayBind !== undefined) {
    if (!/^[\w.:[\]-]+:\d{1,5}$/.test(s.gatewayBind)) throw new Error("gateway bind must be host:port");
    t = setTomlValue(t, "gateway", "bind", s.gatewayBind);
  }
  if (s.paymentsEnabled !== undefined) t = setTomlValue(t, "payments", "enabled", s.paymentsEnabled);
  if (s.paymentsRail !== undefined) {
    if (s.paymentsRail !== "coinpay") throw new Error("coinpay is the only payment rail");
    t = setTomlValue(t, "payments", "rail", s.paymentsRail);
  }
  if (s.apiKeyEnv !== undefined) {
    if (!isEnvVarName(s.apiKeyEnv)) throw new Error("api_key_env is the NAME of an environment variable (e.g. COINPAY_API_KEY), not the key");
    t = setTomlValue(t, "payments.coinpay", "api_key_env", s.apiKeyEnv);
  }
  if (s.recipientEmail !== undefined) {
    if (s.recipientEmail && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(s.recipientEmail)) throw new Error("recipient email is not an email address");
    t = s.recipientEmail
      ? setTomlValue(t, "payments.coinpay", "recipient_email", s.recipientEmail)
      : unsetTomlValue(t, "payments.coinpay", "recipient_email");
  }
  if (s.payoutWallet !== undefined) {
    t = s.payoutWallet ? setTomlValue(t, "hosting", "payout_wallet", s.payoutWallet.trim()) : unsetTomlValue(t, "hosting", "payout_wallet");
  }
  if (s.payoutCoin !== undefined && s.payoutCoin) t = setTomlValue(t, "hosting", "payout_coin", s.payoutCoin.trim());
  const check = checkConfigText(t);
  if (!check.ok) throw new Error(check.error);
  return t;
}
