import { readFileSync, readdirSync } from "node:fs";
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

export interface PluginManifest {
  id: string;
  name: string;
  version: string;
  kind: "workload" | "service" | "sdk";
  category: Category;
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
  };
  workloads?: Record<string, { command?: string; validation?: string }>;
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
    description: String(m.description ?? ""),
    author: typeof m.author === "string" ? m.author : undefined,
    license: typeof m.license === "string" ? m.license : undefined,
    homepage: typeof m.homepage === "string" ? m.homepage : undefined,
    source: typeof m.source === "string" ? m.source : undefined,
    keywords: Array.isArray(m.keywords) ? (m.keywords as string[]) : undefined,
    requirements: m.requirements as PluginManifest["requirements"],
    workloads: m.workloads as PluginManifest["workloads"],
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

/**
 * Group plugins for display, preserving CATEGORIES order and dropping
 * groups that are empty.
 */
export function byCategory(
  plugins: PluginManifest[],
): { category: Category; plugins: PluginManifest[] }[] {
  return CATEGORIES.map((category) => ({
    category,
    plugins: plugins.filter((p) => p.category === category),
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
 * Install command shown on each plugin card. Uniform across plugins:
 * `c0mpute plugin install <id>`. The CLI resolves the id to
 * `https://c0mpute.com/plugins/<id>/install.sh`. For in-process plugins
 * (transcode) the script informs the user it's built into c0mpute.
 */
export function installCommand(p: PluginManifest): string {
  return `c0mpute plugin install ${p.id}`;
}
