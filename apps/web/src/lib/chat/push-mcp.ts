// MCP tools for `/push hosting`: the same per-account nodes and pushes as the
// chat and /api/v1/hosting, for MCP clients signed in with a c0k_ key.
import { chooseNode, nodeJob, publicNode, publicPush, PushError, SITE_NAME, validatePushFiles, type PushStore } from "./push";

export const HOSTING_TOOLS = [
  {
    name: "hosting_nodes",
    description:
      "The hosting nodes linked to this c0mpute account (link one by running `c0mpute chat connect hosting --name <node>` on it). A node publishes your pushes with its own key and pays hosts through CoinPay.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "hosting_push",
    description:
      "Publish files as a site on the c0mpute network through one of your linked nodes (`hosting publish --disks auto`, paid hosts via CoinPay). Pushing the same name again ships a new version. Files are inline text (`text`, up to 1 MiB in all) or c0mpute chat results (`src`: /api/v1/images/<id>, /api/v1/music/<id>[/cover], /api/v1/album/<id>/zip|cover, /api/v1/promo/<id>/video|captions). Returns at once; poll hosting_push_status.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Site name: 1-63 of a-z, 0-9 and '-'." },
        node: { type: "string", description: "Which linked node publishes it (default: the one seen most recently, or the node that signed this site)." },
        files: {
          type: "array",
          items: {
            type: "object",
            properties: {
              path: { type: "string", description: "Path in the site, e.g. index.html or music/01 - Song.mp3" },
              type: { type: "string", description: "MIME type" },
              text: { type: "string" },
              src: { type: "string" },
            },
            required: ["path"],
          },
        },
      },
      required: ["name", "files"],
    },
  },
  {
    name: "hosting_push_status",
    description: "A push's status (queued, publishing, live, failed, deleting), the node's latest progress line, and the site url once live.",
    inputSchema: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
  },
  {
    name: "hosting_pushes",
    description: "Every site this account has pushed, newest first.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "hosting_push_rm",
    description: "Take a pushed site off the network (its node runs `hosting rm`). Poll hosting_push_status until it reports not found.",
    inputSchema: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
  },
] as const;

export const HOSTING_TOOL_NAMES = new Set<string>(HOSTING_TOOLS.map((t) => t.name));

type Result = { text: string; isError?: boolean };
const out = (v: unknown): Result => ({ text: JSON.stringify(v) });
const err = (t: string): Result => ({ text: t, isError: true });

export async function hostingTool(name: string, args: Record<string, unknown>, userId: string, ps: PushStore): Promise<Result> {
  try {
    if (name === "hosting_nodes") return out({ nodes: (await ps.nodes(userId)).map((n) => publicNode(n)) });
    if (name === "hosting_pushes") {
      const [pushes, nodes] = await Promise.all([ps.pushes(userId, "hosting"), ps.nodes(userId)]);
      const byId = new Map(nodes.map((n) => [n.id, n]));
      return out({ pushes: pushes.map((p) => publicPush(p, byId.get(p.node_id))) });
    }
    const site = String(args.name ?? "").toLowerCase();
    if (!SITE_NAME.test(site)) return err("name: 1-63 of a-z, 0-9 and '-'");
    if (name === "hosting_push_status") {
      const p = await ps.push(userId, "hosting", site);
      if (!p) return err(`no push named ${site} on this account`);
      return out({ push: publicPush(p, await ps.node(userId, p.node_id)) });
    }
    if (name === "hosting_push_rm") {
      const r = await ps.deletePush(userId, "hosting", site);
      if (!r) return err(`no push named ${site} on this account`);
      return out({ ok: true, status: r === "gone" ? "deleted" : "deleting", next: r === "gone" ? undefined : "poll hosting_push_status until it is not found" });
    }
    if (name === "hosting_push") {
      const files = validatePushFiles(args.files);
      if (typeof files === "string") return err(files);
      const existing = await ps.push(userId, "hosting", site);
      const node = chooseNode(await ps.nodes(userId), args.node ?? (existing?.site_id ? existing.node_id : undefined));
      if ("error" in node) return err(node.error);
      const p = await ps.upsertPush(userId, { target: "hosting", name: site, node, files, disks: "auto" });
      return out({ push: publicPush(p, node), job: { action: nodeJob(p).action, files: files.length }, next: "poll hosting_push_status every 5-10 s" });
    }
    return err(`unknown tool: ${name}`);
  } catch (e) {
    return err(e instanceof PushError ? e.message : `push store unavailable: ${e instanceof Error ? e.message : e}`);
  }
}
