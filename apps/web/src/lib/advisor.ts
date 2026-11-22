// Agentic Advisor: Finance (commands/*.md with `pack: finance`): links and
// copy shared by the landing page (/advisor/finance), shared chats and docs.
// Referral handles ride the EXISTING c0mpute referral program (/r/<code>,
// `c0mpute worker register --ref`); there is no second commission system.
import { sanitizeRef } from "./nodes-core";

/** Profullstack's own handle: the default when a link has none. */
export const DEFAULT_REF = "profullstack";

export const ADVISOR_PATH = "/advisor/finance";

const site = () => (process.env.C0MPUTE_SITE_URL || "https://c0mpute.com").replace(/\/$/, "");

/** https://c0mpute.com/advisor/finance?ref=<handle> */
export function advisorLink(ref?: string | null): string {
  return `${site()}${ADVISOR_PATH}?ref=${sanitizeRef(ref) ?? DEFAULT_REF}`;
}

/** https://c0mpute.com/r/<handle>: the referral landing every handle already has. */
export function referralLink(ref?: string | null): string {
  return `${site()}/r/${sanitizeRef(ref) ?? DEFAULT_REF}`;
}
