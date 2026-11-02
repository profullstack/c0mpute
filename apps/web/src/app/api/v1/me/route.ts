import { currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ id: user.id, did: user.did, name: user.name, auth: user.token_kind });
}
