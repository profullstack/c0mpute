import { describe, expect, test } from "bun:test";
import { BUILT_WITH, GALLERY } from "./gallery";
import { SOLUTIONS, solutionBySlug, solutionForLabel, solutionPath } from "./solutions";

/**
 * The Solutions menu and pages are the gallery's "built with" vocabulary, one
 * page each. Adding a feature to BUILT_WITH without a solution page (or the
 * reverse) fails here instead of leaving a label with nowhere to go.
 */
describe("solutions", () => {
  test("one solution per BUILT_WITH label, in the same order", () => {
    expect(SOLUTIONS.map((s) => s.label)).toEqual([...BUILT_WITH]);
  });

  test("slugs are unique, URL-safe and resolve", () => {
    const slugs = SOLUTIONS.map((s) => s.slug);
    expect(new Set(slugs).size).toBe(slugs.length);
    for (const s of SOLUTIONS) {
      expect(s.slug).toMatch(/^[a-z0-9]+(-[a-z0-9]+)*$/);
      expect(solutionBySlug(s.slug)).toBe(s);
      expect(solutionForLabel(s.label)).toBe(s);
      expect(solutionPath(s)).toBe(`/solutions/${s.slug}`);
    }
    expect(solutionBySlug("nope")).toBeUndefined();
    expect(solutionForLabel("not a feature")).toBeUndefined();
  });

  test("every page has an install, an example and at least two use cases with examples", () => {
    for (const s of SOLUTIONS) {
      expect(s.summary.length).toBeGreaterThan(40);
      expect(s.install.trim().length).toBeGreaterThan(0);
      expect(s.example.trim().length).toBeGreaterThan(0);
      expect(s.result.trim().length).toBeGreaterThan(0);
      expect(s.useCases.length).toBeGreaterThanOrEqual(2);
      const titles = s.useCases.map((u) => u.title);
      expect(new Set(titles).size).toBe(titles.length);
      for (const u of s.useCases) {
        expect(u.text.trim().length).toBeGreaterThan(20);
        expect(u.example.trim().length).toBeGreaterThan(0);
      }
      expect(s.links.length).toBeGreaterThan(0);
      for (const l of s.links) expect(l.href).toMatch(/^(\/|https:\/\/)/);
    }
  });

  test("copy has no em dashes and no placeholder text", () => {
    const text = JSON.stringify(SOLUTIONS);
    expect(text).not.toContain("—");
    expect(text.toLowerCase()).not.toContain("todo");
    expect(text.toLowerCase()).not.toContain("lorem");
  });

  test("every feature a seeded gallery entry names has a solution page", () => {
    for (const e of GALLERY) for (const b of e.built_with) expect(solutionForLabel(b)).toBeDefined();
  });
});
