// Who started which generation job (image, video, song, album, promo).
//
// The results are fetched by job id from Infernet, which does not know which
// c0mpute account a job belongs to. Without a record here, any signed-in
// account that learned a job id could fetch someone else's result. One row per
// job, written when the job starts; every /api/v1/<kind>/<id> route asks
// `allowed` first.
//
// Jobs started before this table existed have no row. They stay reachable
// ("unknown" ids are allowed) unless `strict` is on, so old conversations keep
// their media; a job that IS recorded is only ever served to its owner.

import type { Sql } from "./store";

export const MEDIA_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_media_jobs (
     job_id     TEXT PRIMARY KEY,
     user_id    TEXT NOT NULL,
     kind       TEXT NOT NULL,
     created_at TEXT NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_chat_media_jobs_user ON chat_media_jobs(user_id)`,
];

export type MediaKind = "image" | "video" | "music" | "album" | "promo";

const JOB_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function createMediaStore(sql: Sql, opts: { strict?: boolean } = {}) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of MEDIA_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const store = {
    init,

    /** Record that `userId` started this job. First writer wins: a job never changes hands. */
    async record(userId: string, kind: MediaKind, jobId: unknown): Promise<void> {
      if (typeof jobId !== "string" || !JOB_ID.test(jobId)) return;
      await init();
      await sql.run(`INSERT OR IGNORE INTO chat_media_jobs (job_id, user_id, kind, created_at) VALUES (?, ?, ?, ?)`, [
        jobId.toLowerCase(),
        userId,
        kind,
        new Date().toISOString(),
      ]);
    },

    async owner(jobId: string): Promise<string | null> {
      await init();
      const r = (await sql.all(`SELECT user_id FROM chat_media_jobs WHERE job_id = ?`, [jobId.toLowerCase()]))[0];
      return r ? String(r.user_id) : null;
    },

    /** May `userId` read this job's result? Its owner: yes. Someone else's: never. Unrecorded: unless strict. */
    async allowed(userId: string, jobId: string): Promise<boolean> {
      const owner = await store.owner(jobId);
      if (owner) return owner === userId;
      return !opts.strict;
    },
  };
  return store;
}

export type MediaStore = ReturnType<typeof createMediaStore>;
