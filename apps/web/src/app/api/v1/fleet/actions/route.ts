import { isFleetAction, runAction, selectNodes } from "@/lib/accounts/fleet";
import { accountStore, fleetFor, fleetService } from "@/lib/accounts/server";
import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { earningsStore } from "@/lib/earnings/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/fleet/actions {action, nodes?: [ids] | tag?: "rack-a" | all?: true, args?}
 *
 * Node-side (queued, run by the node on its next poll, about a minute):
 *   pause | resume | update | restart
 *   models {models: string[]} (empty: every model the node has)
 *   house-models | uncensored | hosting | storage {on: true | false | null}
 *   (hosting and storage restart the worker after running jobs finish; null
 *   hands the setting back to the node's own config)
 * Account-side (immediate):
 *   rename {label} | tag {tags} | untag {tags} | set-tags {tags} | cost {cost_usd_per_hour} | unlink
 * Every action is written to the audit log (GET /api/v1/fleet/audit).
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const body = (await req.json().catch(() => ({}))) as { action?: unknown; nodes?: unknown; tag?: unknown; all?: unknown; args?: unknown };
  const action = typeof body.action === "string" ? body.action : "";
  if (!isFleetAction(action)) return json({ error: { message: "action: pause, resume, update, restart, models, house-models, uncensored, hosting, storage, rename, tag, untag, set-tags, cost or unlink", code: 400 } }, 400);
  const fleet = await fleetFor(user.id);
  const targets = selectNodes(fleet.nodes, body);
  if (typeof targets === "string") return json({ error: { message: targets, code: 400 } }, 400);
  if (targets.length === 0) return json({ error: { message: "no nodes matched", code: 404 } }, 404);
  const args = body.args && typeof body.args === "object" && !Array.isArray(body.args) ? (body.args as Record<string, unknown>) : {};
  const r = await runAction({
    userId: user.id,
    actor: user.token_kind === "session" ? "web" : "api",
    action,
    args,
    targets,
    fleet: fleetService(),
    accounts: accountStore(),
    earnings: earningsStore(),
  });
  if ("error" in r) return json({ error: { message: r.error, code: 400 } }, 400);
  return json(r, r.ok > 0 ? 200 : 409);
}
