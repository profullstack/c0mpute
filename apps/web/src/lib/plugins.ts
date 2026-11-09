import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import toml from "@iarna/toml";

/**
 * Build-time loader for plugin manifests.
 *
 * Reads every `plugins/<id>/module.toml` in the repo and exposes them as
 * a typed list. The marketplace page renders from this — no runtime DB,
 * no API. New plugins land via PR.
 */

/**
 * Domain grouping for the marketplace page.
 *
 * Distinct from `kind`, and the two answer different questions. `kind` is
 * what the thing is to the runtime (does c0mpute run it in-process, shell
 * out to it, or is it a service?). `category` is what it is to a person
 * scanning the list. A flat list of 21 entries made the product look
 * unfocused; grouping shows what the network actually does.
 */
export const CATEGORIES = [
  "ai",
  "media",
  "data",
  "infrastructure",
  "applications",
  "other",
] as const;

export type Category = (typeof CATEGORIES)[number];

export const CATEGORY_LABELS: Record<Category, string> = {
  ai: "ai",
  media: "media",
  data: "data / web",
  infrastructure: "infrastructure",
  applications: "services / applications",
  other: "uncategorized",
};

export const CATEGORY_BLURBS: Record<Category, string> = {
  ai: "inference, embeddings, training, speech, vision",
  media: "video, audio, rendering, streaming, publishing",
  data: "crawling, storage, hosting",
  infrastructure: "long-running capacity and generic execution",
  applications:
    "higher-level products built on c0mpute — not protocol dependencies",
  other: "no category declared in module.toml",
};

/**
 * One entry of a manifest's `[module.models]` table: a model a plugin can
 * be asked to run. `vramGib` is the floor a provider has to beat at the
 * stated precision, which is what the marketplace card shows — a reader
 * deciding whether their card can earn on this plugin needs that number,
 * not a benchmark.
 */
export interface ModelOption {
  repo?: string;
  /** Diffusers-format repo the runner loads; `repo` stays the canonical id. */
  diffusers_repo?: string;
  default?: boolean;
  params?: string;
  precision?: string;
  vram_gib?: number;
  resolution?: string;
  workloads?: string[];
  license?: string;
  /** Diffusers pipeline family the runner uses; omitted means "wan". */
  backend?: string;
  /** The clip carries a generated soundtrack, not just pictures. */
  audio?: boolean;
  /** Few-step distilled checkpoint (fixed schedule, no CFG). */
  distilled?: boolean;
}

/**
 * How much of a plugin actually ships. A manifest describes a contract; this
 * says whether the code behind it exists. `available` runs today, `partial`
 * runs with a caveat (see `statusNote`), `coming-soon` is a spec with no
 * implementation yet. A manifest without `status` is treated as available.
 */
export const PLUGIN_STATUSES = ["available", "partial", "coming-soon"] as const;

export type PluginStatus = (typeof PLUGIN_STATUSES)[number];

export interface PluginManifest {
  id: string;
  name: string;
  version: string;
  /** `donation`: gives idle capacity away (e.g. crowdgpt); earns nothing. */
  kind: "workload" | "service" | "sdk" | "donation";
  category: Category;
  status: PluginStatus;
  /** One sentence on what is missing; set for partial plugins. */
  statusNote?: string;
  /**
   * Compiled into the c0mpute binary (`builtin = true` under [module]).
   * Explicit rather than inferred from dispatch mode: an in-process manifest
   * says how the code would run, not that the code exists.
   */
  builtin: boolean;
  /** `plugins/<id>/install.sh` exists, so `c0mpute plugin install <id>` works. */
  hasInstallScript: boolean;
  description: string;
  author?: string;
  license?: string;
  homepage?: string;
  source?: string;
  keywords?: string[];
  requirements?: {
    c0mpute?: string;
    os?: string[];
    arch?: string[];
    capabilities?: string[];
    /** What a node needs to serve it, in words (GPU, docker, public IP). */
    hardware?: string;
  };
  workloads?: Record<string, { command?: string; validation?: string }>;
  models?: Record<string, ModelOption>;
  dispatch?: {
    mode: "in-process" | "subprocess" | "container";
    binary?: string;
    image?: string;
  };
  install?: {
    url?: string;
  };
  surfaces?: {
    cli?: string;
    web?: string;
  };
}

const PLUGINS_DIR = path.resolve(process.cwd(), "..", "..", "plugins");

export function loadAllPlugins(): PluginManifest[] {
  const ids = readdirSync(PLUGINS_DIR, { withFileTypes: true })
    .filter((d) => d.isDirectory())
    .map((d) => d.name);

  return ids
    .map((id) => loadOne(id))
    .filter((p): p is PluginManifest => p !== null)
    .sort((a, b) => a.id.localeCompare(b.id));
}

function loadOne(id: string): PluginManifest | null {
  const file = path.join(PLUGINS_DIR, id, "module.toml");
  let raw: string;
  try {
    raw = readFileSync(file, "utf8");
  } catch {
    return null;
  }
  const parsed = toml.parse(raw) as { module?: Record<string, unknown> };
  const m = parsed.module;
  if (!m) return null;
  return {
    id: String(m.id ?? id),
    name: String(m.name ?? id),
    version: String(m.version ?? "0.0.0"),
    kind: (m.kind as PluginManifest["kind"]) ?? "workload",
    category: toCategory(m.category),
    status: toStatus(m.status),
    statusNote:
      typeof m.status_note === "string" && m.status_note.length > 0
        ? m.status_note
        : undefined,
    builtin: m.builtin === true,
    hasInstallScript: existsSync(path.join(PLUGINS_DIR, id, "install.sh")),
    description: String(m.description ?? ""),
    author: typeof m.author === "string" ? m.author : undefined,
    license: typeof m.license === "string" ? m.license : undefined,
    homepage: typeof m.homepage === "string" ? m.homepage : undefined,
    source: typeof m.source === "string" ? m.source : undefined,
    keywords: Array.isArray(m.keywords) ? (m.keywords as string[]) : undefined,
    requirements: m.requirements as PluginManifest["requirements"],
    workloads: m.workloads as PluginManifest["workloads"],
    models: m.models as PluginManifest["models"],
    dispatch: m.dispatch as PluginManifest["dispatch"],
    install: m.install as PluginManifest["install"],
    surfaces: m.surfaces as PluginManifest["surfaces"],
  };
}

/**
 * A manifest without a recognized `category` groups under "other" rather
 * than failing the build. A miscategorized plugin is a cosmetic problem; a
 * red build on someone's first contribution is not.
 */
function toCategory(value: unknown): Category {
  return CATEGORIES.includes(value as Category) ? (value as Category) : "other";
}

function toStatus(value: unknown): PluginStatus {
  return PLUGIN_STATUSES.includes(value as PluginStatus)
    ? (value as PluginStatus)
    : "available";
}

/**
 * Group plugins for display, preserving CATEGORIES order and dropping
 * groups that are empty.
 */
export function byCategory(
  plugins: PluginManifest[],
): { category: Category; plugins: PluginManifest[] }[] {
  return CATEGORIES.map((category) => ({
    category,
    // Coming-soon cards sort after the ones that run today; each half keeps
    // the incoming (id) order.
    plugins: [
      ...plugins.filter(
        (p) => p.category === category && p.status !== "coming-soon",
      ),
      ...plugins.filter(
        (p) => p.category === category && p.status === "coming-soon",
      ),
    ],
  })).filter((g) => g.plugins.length > 0);
}

/**
 * Tagline shown on plugin cards. Falls back to description's first sentence.
 */
export function tagline(p: PluginManifest): string {
  const first = p.description.split(/[.!?](?:\s|$)/)[0];
  return first.length > 0 ? first : p.description;
}

/**
 * Install command for a plugin, or null when there is nothing to install.
 * `c0mpute plugin install <id>` resolves the id to
 * `https://c0mpute.com/plugins/<id>/install.sh`, so the command is only
 * offered when that script exists, the plugin is not already compiled into
 * the binary, and it is not a coming-soon spec.
 */
export function installCommand(p: PluginManifest): string | null {
  if (p.status === "coming-soon" || p.builtin || !p.hasInstallScript) {
    return null;
  }
  return `c0mpute plugin install ${p.id}`;
}

/** Ships inside the c0mpute binary and works today. */
export function installedByDefault(p: PluginManifest): boolean {
  return p.builtin && p.status === "available";
}

/** Gives idle capacity away rather than selling it; off until the operator opts in. */
export function isDonation(p: PluginManifest): boolean {
  return p.kind === "donation";
}

/**
 * Model options for a card, default first then alphabetical. A plugin with
 * no `[module.models]` table returns an empty list and renders nothing —
 * most plugins have no model to choose.
 */
export function modelOptions(
  p: PluginManifest,
): { id: string; model: ModelOption }[] {
  return Object.entries(p.models ?? {})
    .map(([id, model]) => ({ id, model }))
    .sort(
      (a, b) =>
        Number(b.model.default ?? false) - Number(a.model.default ?? false) ||
        a.id.localeCompare(b.id),
    );
}
