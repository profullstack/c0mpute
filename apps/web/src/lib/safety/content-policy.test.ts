import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { alnumRuns, checkPolicy, hashRequester, normalize, squeeze } from "./content-policy";

// The Rust crate owns the canonical files; this app keeps a byte-identical copy
// of terms.json so the Next build never reaches outside apps/web.
const RUST_DIR = join(import.meta.dir, "../../../../../node/crates/c0mpute-core/src/content_policy");

type Case = { prompt: string; negative?: string; extra?: string[]; adult: boolean; image?: boolean; expect: string };

describe("content policy (shared with the Rust gateway and workers)", () => {
  test("terms.json is identical to the Rust crate's", () => {
    expect(readFileSync(join(import.meta.dir, "content-policy-terms.json"), "utf8")).toBe(
      readFileSync(join(RUST_DIR, "terms.json"), "utf8"),
    );
  });

  test("passes every shared vector", () => {
    const { cases } = JSON.parse(readFileSync(join(RUST_DIR, "vectors.json"), "utf8")) as { cases: Case[] };
    expect(cases.length).toBeGreaterThanOrEqual(30);
    const failures = cases
      .map((c) => {
        const got = checkPolicy({ prompt: c.prompt, negative: c.negative, extra: c.extra, adult: c.adult, hasImageInput: c.image }) ?? "ok";
        return got === c.expect ? null : `${JSON.stringify(c.prompt)} adult=${c.adult} neg=${JSON.stringify(c.negative ?? "")}: want ${c.expect}, got ${got}`;
      })
      .filter(Boolean);
    expect(failures).toEqual([]);
  });

  test("normalization folds lookalikes and invisibles", () => {
    expect(normalize("Ｔｅｅｎ")).toBe("teen");
    expect(normalize("tееn")).toBe("teen");
    expect(normalize("te​en")).toBe("teen");
    expect(normalize("Ñúdé")).toBe("nude");
    expect(squeeze("teeeeen")).toBe("teen");
    expect(alnumRuns("15yo")).toEqual(["15", "yo"]);
  });

  test("requester hashes are short, stable and distinct", () => {
    expect(hashRequester("subject-1")).toHaveLength(12);
    expect(hashRequester("subject-1")).toBe(hashRequester("subject-1"));
    expect(hashRequester("subject-1")).not.toBe(hashRequester("subject-2"));
  });
});
