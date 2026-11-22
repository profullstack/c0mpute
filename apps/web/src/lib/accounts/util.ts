/** Only same-site paths: "//evil.com" and "/\evil.com" are off-site. */
export const safeNext = (raw: string | null | undefined, fallback = "/account") =>
  raw && /^\/(?![\/\\])/.test(raw) && raw.length <= 512 ? raw : fallback;

/** Sign-in errors the /signin page explains (the callback and the forms redirect with ?error=). */
export const SIGNIN_ERRORS: Record<string, string> = {
  no_did:
    "Your CoinPay account has no DID yet. c0mpute uses your CoinPay DID as your identity on the network (and payouts go to your CoinPay wallet). Sign in at coinpayportal.com and create one (Reputation, then DID), or run `coinpay did claim` (also `c0mpute coinpay reputation did setup`), then sign in again. Or sign in with a passkey now and connect CoinPay later from your account page.",
  coinpay_taken:
    "That CoinPay account is already connected to another c0mpute account. Sign in with CoinPay to use that account, or disconnect CoinPay there first. Accounts are never merged.",
  state: "The sign-in took too long or was started in another tab. Please try again.",
  signin: "CoinPay sign-in did not go through. Please try again.",
  access_denied: "You declined the CoinPay consent screen, so nothing was shared. Try again when ready.",
  session: "Sign in first.",
};
