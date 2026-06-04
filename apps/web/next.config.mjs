import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // better-sqlite3 is a native module; keep it out of the bundle.
  serverExternalPackages: ["better-sqlite3"],
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

  async redirects() {
    return [
      // www.c0mpute.com → c0mpute.com (any path).
      {
        source: "/:path*",
        has: [{ type: "host", value: "www.c0mpute.com" }],
        destination: "https://c0mpute.com/:path*",
        permanent: true,
      },
      // /releases/latest/<artifact> → GitHub Releases latest download.
      // GitHub maintains the redirect from `latest/download/<file>` to
      // whichever the most recent release is, so we don't have to track
      // versions on c0mpute.com.
      {
        source: "/releases/latest/:artifact",
        destination:
          "https://github.com/profullstack/c0mpute/releases/latest/download/:artifact",
        permanent: false,
      },
      // /releases/<version>/<artifact> → GitHub Releases pinned version.
      {
        source: "/releases/:version/:artifact",
        destination:
          "https://github.com/profullstack/c0mpute/releases/download/:version/:artifact",
        permanent: false,
      },
    ];
  },
};

export default nextConfig;
