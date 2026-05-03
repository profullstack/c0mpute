import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // c0mpute.com landing — served at the apex. Per-plugin dashboards
  // (transcode, coinpay, infernet) will mount as separate Next apps
  // under /transcode, /coinpay, /infernet once we build them; for v1
  // the CLI is the entire UX.
  trailingSlash: false,

  // Bun monorepo: Turbopack can't infer the workspace root reliably,
  // so pin it to the repo root explicitly. Without this, Next 16 errors
  // with "We couldn't find the Next.js package" on Railway builds.
  turbopack: {
    root: path.resolve(__dirname, "..", ".."),
  },

  // 308 www.c0mpute.com → c0mpute.com (any path).
  async redirects() {
    return [
      {
        source: "/:path*",
        has: [{ type: "host", value: "www.c0mpute.com" }],
        destination: "https://c0mpute.com/:path*",
        permanent: true,
      },
    ];
  },
};

export default nextConfig;
