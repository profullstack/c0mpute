import { defineConfig } from "drizzle-kit";

export default defineConfig({
  schema: "./apps/web/src/lib/schema.ts",
  out: "./drizzle",
  dialect: "sqlite",
  dbCredentials: {
    url: process.env.SQLITE_CLOUD_URL ?? "file:./blogs.db",
  },
});
