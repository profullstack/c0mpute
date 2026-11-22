// Knobs for "Get listed" on the c0mpute gallery. Prices and waits are
// Anthony's to change: each is one value here.

export const GALLERY_CONFIG = {
  /** Priority tier: one-time price in USD, paid through CoinPay. */
  priorityPriceUsd: 29,
  /** Free tier without a badge: days in the queue before the listing goes live. */
  freeQueueDays: 30,
  /** Approved free listings that went live on the badge are re-checked this often. */
  badgeRecheckDays: 7,
  /** A badge must be missing on two checks this far apart before the listing goes back to the queue. */
  badgeMissingGraceHours: 24,
  /** Queued free listings: look for a newly placed badge this often, without anyone pressing the button. */
  queuedBadgeScanHours: 24,
  /** "Check my badge now": at most once per listing per this many seconds. */
  manualCheckCooldownSeconds: 60,
  /** Submissions per IP per hour. */
  submitsPerHour: 10,
  /** Background pass (queue promotion, badge re-checks): every this many minutes. */
  tickMinutes: 60,
  /** Listings handled per kind per pass, so one pass never hammers dozens of sites. */
  tickBatch: 20,
} as const;

/** The public origin badges, links and emails point at. */
export const GALLERY_ORIGIN = "https://c0mpute.com";
