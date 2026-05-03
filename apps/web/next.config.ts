import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  // Quest mounts under depin.quest/video. basePath rewrites all internal
  // links and asset URLs; assetPrefix keeps static assets under /video too.
  basePath: "/video",
  assetPrefix: "/video",
  trailingSlash: false,
};

export default nextConfig;
