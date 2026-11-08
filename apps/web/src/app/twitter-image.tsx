import { ogCard, OG_SIZE } from "@/lib/og";

export const alt = "c0mpute: the open compute network";
export const size = OG_SIZE;
export const contentType = "image/png";

export default function Image() {
  return ogCard({
    kicker: "run jobs anywhere, sell unused compute",
    title: "The open compute network",
    sub: "CPUs + GPUs from PCs to data centers",
  });
}
