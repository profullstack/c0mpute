import { ogCard, OG_SIZE } from "@/lib/og";
import { shareStore } from "@/lib/chat/server";

export const alt = "A conversation shared from c0mpute chat";
export const size = OG_SIZE;
export const contentType = "image/png";

/** The preview card in a text message: the chat's question as the title, so it reads as what it is. */
export default async function Image({ params }: { params: Promise<{ id: string }> }) {
  const share = await shareStore()
    .get((await params).id)
    .catch(() => null);
  const title = share?.title ?? "A shared conversation";
  return ogCard({
    kicker: "shared chat",
    title: title.length > 90 ? `${title.slice(0, 89).replace(/\s+\S*$/, "")}…` : title,
    sub: "c0mpute chat · tap to read · no sign-in needed",
  });
}
