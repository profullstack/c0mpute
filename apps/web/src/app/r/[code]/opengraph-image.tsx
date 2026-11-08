import { ogCard, OG_SIZE } from "@/lib/og";
import { sanitizeRef } from "@/lib/nodes";

export const alt = "Run a c0mpute node";
export const size = OG_SIZE;
export const contentType = "image/png";

export default async function Image({ params }: { params: Promise<{ code: string }> }) {
  const code = sanitizeRef((await params).code);
  return ogCard({
    kicker: code ? `${code} invited you` : "you're invited",
    title: "Run a c0mpute node",
    sub: "one command · CPU or GPU · goal: 100,000 nodes",
  });
}
