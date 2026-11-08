import { ogCard, OG_SIZE } from "@/lib/og";

export const alt = "c0mpute chat: open models on the open compute network";
export const size = OG_SIZE;
export const contentType = "image/png";

export default function Image() {
  return ogCard({
    kicker: "open models, served by people's own hardware",
    title: "Chat on c0mpute",
    sub: "browser · CLI · OpenAI-compatible API · MCP",
  });
}
