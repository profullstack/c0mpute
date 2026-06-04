import type { MetadataRoute } from "next";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: [
      {
        userAgent: "*",
        allow: "/",
        disallow: ["/_next/", "/api/"],
      },
      // Explicitly allow major AI crawlers
      { userAgent: "GPTBot",            allow: "/" },
      { userAgent: "ClaudeBot",         allow: "/" },
      { userAgent: "PerplexityBot",     allow: "/" },
      { userAgent: "Google-Extended",   allow: "/" },
      { userAgent: "OAI-SearchBot",     allow: "/" },
      { userAgent: "CCBot",             allow: "/" },
      { userAgent: "Applebot-Extended", allow: "/" },
    ],
    sitemap: "https://c0mpute.com/sitemap.xml",
  };
}
