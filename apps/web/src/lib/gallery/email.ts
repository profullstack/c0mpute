// Emails to a listing's contact: received, live, badge missing, not listed.
// Sent through Resend (RESEND_API_KEY, EMAIL_FROM) with the network
// cross-promo block. A send never fails the request that caused it.

import { promoBlock } from "@profullstack/cross-promo";
import { badgeSnippet } from "./badge";
import { GALLERY_CONFIG, GALLERY_ORIGIN } from "./config";
import type { Listing } from "./store";

const env = (name: string): string => (process.env as Record<string, string | undefined>)[name] ?? "";

export type GalleryMail = { to: string; subject: string; text: string; html: string };
export type Mailer = (m: GalleryMail) => Promise<void>;

export const statusUrl = (l: Listing) => `${GALLERY_ORIGIN}/gallery/status/${l.id}`;
const day = (iso: string | null) => (iso ? new Date(iso).toUTCString().slice(0, 16) : "");

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

export type MailKind = "submitted" | "approved" | "badge_missing" | "rejected";

/** The body of each email, as plain paragraphs (the HTML is built from the same text). */
export function mailBody(kind: MailKind, l: Listing): { subject: string; paras: string[]; code?: string } {
  const where = `${l.name} (${l.url})`;
  switch (kind) {
    case "submitted":
      if (l.status === "awaiting_payment")
        return {
          subject: `Finish your c0mpute gallery listing: ${l.name}`,
          paras: [
            `We have ${where}. You picked priority review ($${GALLERY_CONFIG.priorityPriceUsd}, one time, paid with CoinPay). It goes live as soon as the payment confirms and the automated checks pass.`,
            `Pay or switch to the free tier here: ${statusUrl(l)}`,
            `Free instead: put the badge below on your site and it goes live the moment we see it.`,
          ],
          code: badgeSnippet(l.url),
        };
      if (l.status === "approved")
        return {
          subject: `You are listed on the c0mpute gallery: ${l.name}`,
          paras: [`We found the badge on ${l.url}, so ${l.name} is live now at ${GALLERY_ORIGIN}/gallery#${l.slug}.`, `Status: ${statusUrl(l)}`],
        };
      return {
        subject: `In the queue for the c0mpute gallery: ${l.name}`,
        paras: [
          `We have ${where}. It goes live on ${day(l.goes_live_at)} after the ${GALLERY_CONFIG.freeQueueDays}-day queue.`,
          `Skip the wait: put this badge on your site and it goes live as soon as we see it (we look every day, or press "check my badge now" on the status page):`,
          `Status and the check button: ${statusUrl(l)}`,
        ],
        code: badgeSnippet(l.url),
      };
    case "approved":
      return {
        subject: `You are listed on the c0mpute gallery: ${l.name}`,
        paras: [
          `${l.name} is live at ${GALLERY_ORIGIN}/gallery#${l.slug}, and in the public feed every gallery reads (${GALLERY_ORIGIN}/api/v1/gallery, shown on infernetprotocol.com/gallery too).`,
          l.listed_via === "badge"
            ? `It is listed on the badge, so keep the badge on your site: we re-check it every ${GALLERY_CONFIG.badgeRecheckDays} days.`
            : `Show it off with the badge:`,
          `Status: ${statusUrl(l)}`,
        ],
        code: badgeSnippet(l.url),
      };
    case "badge_missing":
      return {
        subject: `Your c0mpute gallery badge is gone: ${l.name} is back in the queue`,
        paras: [
          `We could not find the c0mpute gallery badge on ${l.url} on two checks a day apart, so ${l.name} has moved back to the free queue and goes live again on ${day(l.goes_live_at)}.`,
          `Put the badge back and press "check my badge now" to go live again right away: ${statusUrl(l)}`,
        ],
        code: badgeSnippet(l.url),
      };
    case "rejected":
      return {
        subject: `Your c0mpute gallery submission: ${l.name}`,
        paras: [
          `We did not list ${where}.${l.reject_reason ? ` Reason: ${l.reject_reason}` : ""}`,
          l.paid_at ? `You paid for priority review; reply to this email and we will refund it.` : `Questions: reply to this email.`,
        ],
      };
  }
}

export async function renderMail(kind: MailKind, l: Listing): Promise<GalleryMail | null> {
  if (!l.contact_email) return null;
  const b = mailBody(kind, l);
  let promo = { text: "", html: "" };
  try {
    promo = await promoBlock({ site: "c0mpute.com", audience: ["makers", "developers"], seed: l.contact_email });
  } catch {
    // The catalog is optional; the email is not.
  }
  const text = [...b.paras.slice(0, 2), ...(b.code ? [b.code] : []), ...b.paras.slice(2), "c0mpute, the peer-to-peer compute network", promo.text]
    .filter(Boolean)
    .join("\n\n");
  const p = (s: string) => `<p style="margin:0 0 14px;line-height:1.55">${esc(s).replace(/(https:\/\/[^\s)]+)/g, '<a href="$1">$1</a>')}</p>`;
  const html = `<div style="font-family:ui-sans-serif,system-ui,sans-serif;font-size:15px;color:#111;max-width:600px">
${b.paras.slice(0, 2).map(p).join("\n")}
${b.code ? `<pre style="white-space:pre-wrap;word-break:break-all;background:#f4f4f5;border:1px solid #e4e4e7;border-radius:6px;padding:10px;font-size:12px">${esc(b.code)}</pre>` : ""}
${b.paras.slice(2).map(p).join("\n")}
<p style="margin:18px 0 0;color:#666;font-size:13px">c0mpute, the peer-to-peer compute network · <a href="${GALLERY_ORIGIN}/gallery">gallery</a></p>
${promo.html}
</div>`;
  return { to: l.contact_email, subject: b.subject, text, html };
}

/** Resend, from hello@c0mpute.com. */
export const resendMailer: Mailer = async (m) => {
  const key = env("RESEND_API_KEY");
  if (!key) throw new Error("RESEND_API_KEY not set");
  const from = `${env("EMAIL_FROM_NAME") || "c0mpute"} <${env("EMAIL_FROM") || "hello@c0mpute.com"}>`;
  const res = await fetch("https://api.resend.com/emails", {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ from, to: [m.to], subject: m.subject, text: m.text, html: m.html, reply_to: env("EMAIL_FROM") || "hello@c0mpute.com" }),
    signal: AbortSignal.timeout(15_000),
  });
  if (!res.ok) throw new Error(`resend HTTP ${res.status}: ${(await res.text()).slice(0, 200)}`);
};
