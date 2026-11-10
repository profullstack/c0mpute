// Content policy for image and video generation: the TypeScript twin of
// node/crates/c0mpute-core/src/content_policy.rs. Both read the same word
// lists (content-policy-terms.json is a byte-identical copy of the Rust
// crate's terms.json) and pass the same vectors.json, so they cannot drift;
// content-policy.test.ts fails if either does.
//
// Child safety is always on: sexual or nudity terms together with a minor
// indicator are refused, and in adult mode any minor indicator is refused.
// Without adult mode, explicit prompts are refused. Adult mode is text only
// and refuses real or identifiable people. The real-person check is a
// heuristic over common phrasings; it cannot know every name.

import { createHash } from "node:crypto";
import TERMS from "./content-policy-terms.json";

export type RefusalCode =
  | "minor_sexual"
  | "explicit_requires_adult"
  | "real_person"
  | "adult_image_input"
  | "adult_not_enabled"
  | "adult_not_supported"
  | "explicit_output";

export const REFUSAL_MESSAGES: Record<RefusalCode, string> = {
  minor_sexual: "This request was refused by the content policy.",
  explicit_requires_adult: "Explicit content needs adult mode, which is only for verified 18+ accounts.",
  real_person: "Adult mode does not allow real or identifiable people (no deepfakes).",
  adult_image_input: "Adult mode is text only: image inputs are not allowed.",
  adult_not_enabled: "Adult mode is not enabled for this account or gateway.",
  adult_not_supported: "Adult mode is not available for this kind of job.",
  explicit_output: "The result was withheld by the content policy (explicit output without adult mode).",
};

export type PolicyRequest = {
  prompt: string;
  negative?: string;
  /** Planner / storyboard / lyric text that steers the same generation. */
  extra?: string[];
  adult?: boolean;
  /** Any image input: img2img, img2vid, a face or style reference. */
  hasImageInput?: boolean;
};

type Terms = typeof TERMS & { age_words: Record<string, number>; ordinal_words: Record<string, number> };
const T = TERMS as Terms;

const HOMOGLYPHS: Record<string, string> = {
  // Cyrillic
  "а": "a", "в": "b", "с": "c", "ԁ": "d", "е": "e", "ё": "e", "є": "e", "һ": "h", "н": "h", "і": "i", "ї": "i",
  "ј": "j", "к": "k", "ӏ": "l", "м": "m", "о": "o", "р": "p", "ԛ": "q", "ѕ": "s", "т": "t", "ц": "u", "ѵ": "v",
  "ԝ": "w", "ш": "w", "х": "x", "у": "y", "з": "z",
  // Greek
  "α": "a", "β": "b", "ε": "e", "η": "n", "ι": "i", "κ": "k", "ν": "v", "ο": "o", "ρ": "p", "τ": "t", "υ": "u",
  "χ": "x", "γ": "y", "ω": "w", "μ": "u", "σ": "s", "ς": "s",
};

const INVISIBLE = /[­͏؜᠎​-‏‪-‮⁠-⁤﻿]/gu;

/** Lowercase, compatibility-decompose, strip accents and invisibles, fold lookalikes. */
export function normalize(text: string): string {
  const decomposed = String(text ?? "").toLowerCase().normalize("NFKD").replace(INVISIBLE, "").replace(/\p{M}/gu, "");
  let out = "";
  for (const c of decomposed) out += (HOMOGLYPHS[c] ?? c).toLowerCase();
  return out;
}

const LEET: Record<string, string> = { "0": "o", "1": "i", "3": "e", "4": "a", "5": "s", "7": "t", "8": "b", "@": "a", $: "s" };
const leet = (c: string) => LEET[c] ?? c;

/** Runs of 3+ identical characters squeezed to 2. */
export function squeeze(word: string): string {
  let out = "";
  let last = "";
  let run = 0;
  for (const c of word) {
    run = c === last ? run + 1 : 1;
    last = c;
    if (run <= 2) out += c;
  }
  return out;
}

function single(word: string): string {
  let out = "";
  let last = "";
  for (const c of word) {
    if (c !== last) out += c;
    last = c;
  }
  return out;
}

const isWordChar = (c: string) => /^[a-z0-9]$/.test(c);

/** Digit and letter runs: `15yo` -> ["15", "yo"]. */
export function alnumRuns(word: string): string[] {
  return word.match(/[0-9]+|[^0-9]+/g) ?? [];
}

type Token = { word: string; blob: boolean; runs: string[] };
type Analysis = { tokens: Token[]; handle: boolean };

function analyse(text: string): Analysis {
  const norm = normalize(text);
  const chars = [...norm];
  const handle = chars.some((c, i) => {
    if (c !== "@" || (i > 0 && isWordChar(chars[i - 1]))) return false;
    let n = 0;
    for (let j = i + 1; j < chars.length && (isWordChar(chars[j]) || chars[j] === "_"); j++) n++;
    return n >= 2;
  });
  const raw: [string, boolean][] = [];
  for (const chunk of norm.split(/\s+/u).filter(Boolean)) {
    const mapped = [...chunk].map((c) => (c === "@" || c === "$" ? leet(c) : c)).join("");
    const parts = mapped.split(/[^a-z0-9]/).filter(Boolean);
    if (parts.length >= 2 && parts.every((p) => [...p].length === 1)) raw.push([parts.join(""), true]);
    else for (const p of parts) raw.push([p, false]);
  }
  const joined: [string, boolean][] = [];
  for (let i = 0; i < raw.length; ) {
    let j = i;
    while (j < raw.length && !raw[j][1] && [...raw[j][0]].length === 1) j++;
    if (j - i >= 3) {
      joined.push([raw.slice(i, j).map(([w]) => w).join(""), true]);
      i = j;
    } else {
      joined.push(raw[i]);
      i++;
    }
  }
  const tokens = joined.map(([w, blob]) => {
    const folded = /[a-z]/.test(w) ? [...w].map(leet).join("") : w;
    return { word: squeeze(folded), blob, runs: alnumRuns(w).map(squeeze) };
  });
  return { tokens, handle };
}

function wordMatches(word: string, term: string): boolean {
  if (term.endsWith("*")) return word.startsWith(term.slice(0, -1));
  if (word === term || word === term + "s" || word === term + "es") return true;
  const w = single(word);
  const t = single(term);
  return t.length >= 4 && (w === t || w === t + "s");
}

function blobContains(word: string, term: string): boolean {
  const stem = term.replace(/\*+$/, "");
  return !stem.includes(" ") && stem.length >= 3 && word.includes(stem);
}

function words(a: Analysis): [string, boolean][] {
  const out: [string, boolean][] = [];
  for (const t of a.tokens) {
    out.push([t.word, t.blob]);
    if (t.runs.length > 1) for (const r of t.runs) if (/^[a-z]+$/.test(r)) out.push([r, false]);
  }
  return out;
}

function hasTerm(a: Analysis, terms: string[]): boolean {
  return words(a).some(([w, blob]) =>
    terms.some((t) => (t.includes(" ") ? wordMatches(w, t.replaceAll(" ", "")) : wordMatches(w, t) || (blob && blobContains(w, t)))),
  );
}

function hasPhrase(a: Analysis, phrases: string[]): boolean {
  const seq = a.tokens.map((t) => t.word);
  const all = words(a);
  return phrases.some((phrase) => {
    const parts = phrase.split(" ");
    const joinedPhrase = parts.join("");
    if (all.some(([w, blob]) => wordMatches(w, joinedPhrase) || (blob && blobContains(w, joinedPhrase)))) return true;
    for (let i = 0; i + parts.length <= seq.length; i++) {
      if (parts.every((p, k) => wordMatches(seq[i + k], p))) return true;
    }
    return false;
  });
}

function hasMinorAge(a: Analysis): boolean {
  const runs = a.tokens.flatMap((t) => t.runs);
  const number = (s: string): number | null => {
    if (/^[0-9]{1,3}$/.test(s)) return Number(s);
    return Object.hasOwn(T.age_words, s) ? T.age_words[s] : null;
  };
  const is = (list: string[], s: string | undefined) => s !== undefined && list.includes(s);
  for (let i = 0; i < runs.length; i++) {
    const run = runs[i];
    if (Object.hasOwn(T.ordinal_words, run) && T.ordinal_words[run] <= 12 && is(T.grade_words, runs[i + 1])) return true;
    const n = number(run);
    if (n === null) continue;
    const next = runs[i + 1];
    const prev = i >= 1 ? runs[i - 1] : undefined;
    const prev2 = i >= 2 ? runs[i - 2] : undefined;
    if (n <= 17 && is(T.age_units, next)) return true;
    if (n <= 17 && is(T.age_prefix, prev)) return true;
    if (n <= 18 && (is(T.under_words, prev) || is(T.under_words, prev2))) return true;
    const afterSuffix = is(T.ordinal_suffixes, next) ? runs[i + 2] : next;
    if (n <= 12 && is(T.grade_words, afterSuffix)) return true;
  }
  return false;
}

const minor = (a: Analysis) => hasTerm(a, T.minor) || hasPhrase(a, T.minor_phrases) || hasMinorAge(a);
const sexual = (a: Analysis) => hasTerm(a, T.sexual) || hasPhrase(a, T.sexual_phrases);
const explicit = (a: Analysis) => hasTerm(a, T.explicit) || hasPhrase(a, T.explicit_phrases);
const realPerson = (a: Analysis) => a.handle || hasTerm(a, T.real_person) || hasPhrase(a, T.real_person_phrases);

/** null when the request may run, else the refusal code. */
export function checkPolicy(req: PolicyRequest): RefusalCode | null {
  const pos = analyse([req.prompt ?? "", ...(req.extra ?? [])].join("\n"));
  const neg = analyse(req.negative ?? "");
  // A negative prompt steers away from what it names: excluding clothes asks
  // for nudity, excluding adult features asks for a younger subject.
  const undress = hasTerm(neg, T.undress_negative);
  const isSexual = sexual(pos) || undress;
  const isMinor = minor(pos) || hasTerm(neg, T.adult_negative);
  if (hasTerm(pos, T.minor_standalone) || (isMinor && (isSexual || req.adult))) return "minor_sexual";
  if (req.adult) {
    if (req.hasImageInput) return "adult_image_input";
    if (realPerson(pos)) return "real_person";
  } else if (explicit(pos) || undress) {
    return "explicit_requires_adult";
  }
  return null;
}

/** A short, non-reversible id for a requester (same as the Rust side). */
export function hashRequester(id: string): string {
  return createHash("sha256").update(`c0mpute-policy:${id}`).digest("hex").slice(0, 12);
}

const counts = new Map<RefusalCode, number>();

/** Count and log a refusal by reason code with a hashed requester. Never logs the prompt. */
export function recordRefusal(code: RefusalCode, requester: string | null): void {
  counts.set(code, (counts.get(code) ?? 0) + 1);
  console.warn(`content policy refusal reason=${code} side=web requester=${requester ? hashRequester(requester) : "-"}`);
}

export function refusalCounts(): Record<string, number> {
  return Object.fromEntries(counts);
}
