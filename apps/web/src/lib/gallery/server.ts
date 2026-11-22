// Server wiring for the gallery: the store on SQLite Cloud, CoinPay checkout
// and webhook keys, Resend mail, the admin check and the background pass.

import { CoinPay } from "@profullstack/coinpay-onboard";
import { GALLERY, type GalleryEntry } from "../gallery";
import { balanceFor, cloudSql, env, type currentUser } from "../chat/server";
import { takeHits } from "./badge";
import { GALLERY_CONFIG } from "./config";
import { resendMailer } from "./email";
import { createGalleryService, type CheckoutOpener, type GalleryService } from "./service";
import { createGalleryStore, toEntry } from "./store";

/** COINPAY_API_URL may carry an /api suffix (house convention); the client wants the bare origin. */
export const coinpayBase = () => (env("COINPAY_API_URL") || "https://coinpayportal.com").replace(/\/+$/, "").replace(/\/api$/, "");

const checkout: CheckoutOpener = async (o) => {
  const key = env("COINPAY_API_KEY");
  if (!key) throw new Error("COINPAY_API_KEY is not set");
  const cp = new CoinPay({ token: key, base: coinpayBase() });
  const s = await cp.createCheckoutSession({ ...o, property: "c0mpute.com" });
  return { id: s.id, url: s.url };
};

let _service: GalleryService | null = null;
export const gallery = () =>
  (_service ??= createGalleryService({
    store: createGalleryStore(cloudSql),
    mailer: env("RESEND_API_KEY") ? resendMailer : undefined,
    checkout: env("COINPAY_API_KEY") ? checkout : undefined,
  }));

/** Approved entries for the page and the feed; the editorial seed if the database cannot be read. */
export async function galleryEntries(): Promise<{ entries: GalleryEntry[]; live: boolean }> {
  try {
    return { entries: (await gallery().store.approved()).map(toEntry), live: true };
  } catch (e) {
    console.error("gallery: database read failed, serving the seed list", e);
    return { entries: GALLERY.map((e) => ({ ...e, featured: false, listed_via: "editorial" as const })), live: false };
  }
}

type User = NonNullable<Awaited<ReturnType<typeof currentUser>>>;

/**
 * Gallery moderators: CoinPay subjects in C0MPUTE_ADMIN_SUBS (comma separated),
 * or any account Infernet marks admin (INFERNET_ADMIN_USER_IDS, the same
 * accounts that are unmetered in chat).
 */
export async function isGalleryAdmin(user: User | null): Promise<boolean> {
  if (!user || user.token_kind === "oauth") return false;
  const subs = env("C0MPUTE_ADMIN_SUBS").split(",").map((s) => s.trim()).filter(Boolean);
  if (subs.includes(user.coinpay_sub)) return true;
  const b = (await balanceFor(user.coinpay_sub)) as { admin?: unknown } | null;
  return b?.admin === true;
}

let started = false;
/** Badge hit counts flushed every minute; the queue/badge pass every GALLERY_CONFIG.tickMinutes. */
export function startGalleryTicker() {
  if (started) return;
  started = true;
  const flush = setInterval(() => {
    const hits = takeHits();
    if (hits.size) gallery().store.addHits(hits).catch((e) => console.error("gallery: hit flush failed", e));
  }, 60_000);
  (flush as { unref?: () => void }).unref?.();
  let running = false;
  const pass = async () => {
    if (running) return;
    running = true;
    try {
      const r = await gallery().tick();
      if (r.promoted || r.badgeApproved || r.requeued) console.log(`[gallery] pass: ${JSON.stringify(r)}`);
    } catch (e) {
      console.error("gallery: background pass failed", e);
    } finally {
      running = false;
    }
  };
  // First pass a couple of minutes after boot, so a deploy's health check is not competing with it.
  const first = setTimeout(pass, 120_000);
  (first as { unref?: () => void }).unref?.();
  const every = setInterval(pass, GALLERY_CONFIG.tickMinutes * 60_000);
  (every as { unref?: () => void }).unref?.();
}
