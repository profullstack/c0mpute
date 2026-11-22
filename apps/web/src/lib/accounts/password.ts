// The deprecated password method. Offered for devices that cannot hold a
// passkey or open CoinPay; it will be removed. No new password-only features.
//
// argon2id through Bun.password (the site runs on Bun), attempts limited per
// address and per username, and new passwords checked against Have I Been
// Pwned's k-anonymity range API (only the first 5 hex chars of the SHA-1 leave).

import { createHash } from "node:crypto";

export const PASSWORD_DEPRECATION =
  "Passwords are deprecated on c0mpute and will be removed. Add a passkey or connect CoinPay so you keep a way in.";
export const MIN_PASSWORD = 12;
export const MAX_PASSWORD = 256;

/** A sign-in name: 3-32 of a-z 0-9 . _ -, starting with a letter or digit. */
export function validUsername(u: unknown): u is string {
  return typeof u === "string" && /^[a-z0-9][a-z0-9._-]{2,31}$/i.test(u);
}

export function passwordProblem(pw: unknown): string | null {
  if (typeof pw !== "string") return "password required";
  if (pw.length < MIN_PASSWORD) return `at least ${MIN_PASSWORD} characters`;
  if (pw.length > MAX_PASSWORD) return `at most ${MAX_PASSWORD} characters`;
  return null;
}

type BunPassword = {
  hash(pw: string, o: { algorithm: "argon2id"; memoryCost?: number; timeCost?: number }): Promise<string>;
  verify(pw: string, hash: string): Promise<boolean>;
};
const bunPassword = (): BunPassword => {
  const b = (globalThis as { Bun?: { password?: BunPassword } }).Bun?.password;
  if (!b) throw new Error("password hashing needs the Bun runtime");
  return b;
};

export async function hashPassword(pw: string): Promise<string> {
  // 64 MiB, 3 passes: OWASP's argon2id floor is 19 MiB / 2.
  return bunPassword().hash(pw, { algorithm: "argon2id", memoryCost: 65536, timeCost: 3 });
}

export async function verifyPassword(pw: string, hash: string | null | undefined): Promise<boolean> {
  if (!hash || !hash.startsWith("$argon2id$")) return false;
  try {
    return await bunPassword().verify(pw, hash);
  } catch {
    return false;
  }
}

/** A real hash to verify against when the username is unknown, so a miss costs the same time as a wrong password. */
let decoy: Promise<string> | null = null;
export const decoyHash = () => (decoy ??= hashPassword("decoy-password-never-matches-" + Math.random()));

/**
 * How many times this password appears in known breaches, or null when the
 * check could not run (it never blocks a sign-in, only a new password).
 */
export async function breachCount(pw: string, fetcher: typeof fetch = fetch): Promise<number | null> {
  const sha = createHash("sha1").update(pw).digest("hex").toUpperCase();
  try {
    const res = await fetcher(`https://api.pwnedpasswords.com/range/${sha.slice(0, 5)}`, {
      headers: { "add-padding": "true", "user-agent": "c0mpute.com" },
      signal: AbortSignal.timeout(4000),
    });
    if (!res.ok) return null;
    const tail = sha.slice(5);
    for (const line of (await res.text()).split("\n")) {
      const [h, n] = line.trim().split(":");
      if (h === tail) return Number(n) || 0;
    }
    return 0;
  } catch {
    return null;
  }
}
