import type { MetadataRoute } from "next";

const BASE = "https://c0mpute.com";

const staticPages: MetadataRoute.Sitemap = [
  { url: BASE,                         priority: 1.0,  changeFrequency: "weekly"  },
  { url: `${BASE}/getting-started`,    priority: 0.9,  changeFrequency: "monthly" },
  { url: `${BASE}/docs`,               priority: 0.9,  changeFrequency: "weekly"  },
  { url: `${BASE}/plugins`,            priority: 0.8,  changeFrequency: "weekly"  },
  { url: `${BASE}/blog`,               priority: 0.8,  changeFrequency: "daily"   },
  { url: `${BASE}/status`,             priority: 0.6,  changeFrequency: "hourly"  },
  { url: `${BASE}/about`,              priority: 0.7,  changeFrequency: "monthly" },
  { url: `${BASE}/pricing`,            priority: 0.8,  changeFrequency: "monthly" },
  { url: `${BASE}/contact`,            priority: 0.5,  changeFrequency: "monthly" },
  { url: `${BASE}/terms`,              priority: 0.3,  changeFrequency: "yearly"  },
  { url: `${BASE}/privacy`,            priority: 0.3,  changeFrequency: "yearly"  },
];

export default function sitemap(): MetadataRoute.Sitemap {
  return staticPages.map((p) => ({
    ...p,
    lastModified: new Date().toISOString(),
  }));
}
