import { ogCard, OG_SIZE } from "@/lib/og";
import { getNodesPayload } from "@/lib/nodes";

export const alt = "c0mpute nodes online";
export const size = OG_SIZE;
export const contentType = "image/png";
export const dynamic = "force-dynamic";

export default async function Image() {
  const p = await getNodesPayload();
  return ogCard({
    kicker: "every node online now",
    title: `${p.workers_online.toLocaleString()} nodes online`,
    sub: "goal: 100,000 nodes",
  });
}
