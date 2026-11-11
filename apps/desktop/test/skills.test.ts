import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { zipSync, strToU8 } from "fflate";
import {
  createSkill,
  importSkillFolder,
  importSkillZip,
  loadSkills,
  parseFrontMatter,
  readSkill,
  resolveInside,
  skillsPrompt,
  validSkillName,
} from "../src/main/skills";

const FIX = join(import.meta.dir, "fixtures", "skills");
const dirs: string[] = [];
const scratch = () => {
  const d = mkdtempSync(join(tmpdir(), "c0-skills-"));
  dirs.push(d);
  return d;
};
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

describe("SKILL.md front matter", () => {
  test("parses name, folded description and metadata", () => {
    const { data, body } = parseFrontMatter(readFileSync(join(FIX, "hello-world", "SKILL.md"), "utf8"));
    expect(data.name).toBe("hello-world");
    expect(data.description).toBe("Says hello. Use when the user asks for a greeting or a smoke test of skills.");
    expect(data.metadata).toEqual({ author: "test" });
    expect(body.trim().startsWith("# Hello world")).toBe(true);
  });

  test("no front matter: empty data", () => {
    expect(parseFrontMatter("# just text")).toEqual({ data: {}, body: "# just text" });
  });

  test("bad YAML is an error", () => {
    expect(() => parseFrontMatter("---\nname: [unclosed\n---\n")).toThrow("YAML");
  });

  test("description with a colon must be quoted (YAML), and quoted works", () => {
    expect(parseFrontMatter('---\nname: a\ndescription: "Use when: x"\n---\n').data.description).toBe("Use when: x");
  });

  test("names follow the spec", () => {
    expect(validSkillName("agent-email")).toBe(true);
    for (const bad of ["Agent", "a--b", "-a", "a-", "", "a_b", "x".repeat(65)]) expect(validSkillName(bad)).toBe(false);
  });
});

describe("loading ~/skills", () => {
  test("lists skills with scripts, flags a name/folder mismatch, honours disabled", () => {
    const skills = loadSkills(FIX, ["bad-name"]);
    const hello = skills.find((s) => s.name === "hello-world")!;
    expect(hello.enabled).toBe(true);
    expect(hello.error).toBeUndefined();
    expect(hello.scripts).toEqual(["scripts/hi.sh"]);
    const bad = skills.find((s) => s.dir.endsWith("bad-name"))!;
    expect(bad.enabled).toBe(false);
    expect(bad.error).toContain("does not match");
  });

  test("the system prompt lists only enabled, valid skills", () => {
    const p = skillsPrompt(loadSkills(FIX));
    expect(p).toContain("- hello-world: Says hello.");
    expect(p).not.toContain("Not_Valid");
    expect(p).toContain("load_skill");
    expect(skillsPrompt(loadSkills(FIX, ["hello-world"]))).toBe("");
  });

  test("the real ~/skills folder parses (when present)", () => {
    const home = join(process.env.HOME ?? "", "skills");
    if (!existsSync(home)) return;
    for (const s of loadSkills(home)) {
      expect(s.description.length).toBeGreaterThan(0);
    }
  });
});

describe("paths stay inside the skill", () => {
  test("resolveInside refuses escapes", () => {
    const root = join(FIX, "hello-world");
    expect(resolveInside(root, "scripts/hi.sh")).toBe(join(root, "scripts/hi.sh"));
    for (const bad of ["../bad-name/SKILL.md", "/etc/passwd", "scripts/../../x", "", "."]) expect(resolveInside(root, bad)).toBeNull();
  });

  test("a symlink out of the skill is refused", () => {
    const d = scratch();
    writeFileSync(join(d, "outside.txt"), "secret");
    const root = join(d, "s");
    createSkill(d, "s", "x");
    symlinkSync(join(d, "outside.txt"), join(root, "link.txt"));
    expect(resolveInside(root, "link.txt")).toBeNull();
  });
});

describe("adding skills", () => {
  test("create from the template gives a valid skill", () => {
    const d = scratch();
    createSkill(d, "my-skill", "Does the thing. Use when: testing");
    const s = readSkill(join(d, "my-skill"), true);
    expect(s.error).toBeUndefined();
    expect(s.description).toBe("Does the thing. Use when: testing");
    expect(() => createSkill(d, "my-skill", "")).toThrow("exists");
    expect(() => createSkill(d, "Bad Name", "")).toThrow();
  });

  test("import a folder", () => {
    const d = scratch();
    const dest = importSkillFolder(d, join(FIX, "hello-world"));
    expect(readSkill(dest, true).scripts).toEqual(["scripts/hi.sh"]);
  });

  test("import a zip with a top folder, and one without", () => {
    const d = scratch();
    const md = strToU8("---\nname: zipped\ndescription: from a zip\n---\nbody\n");
    importSkillZip(d, zipSync({ "zipped/SKILL.md": md, "zipped/scripts/a.sh": strToU8("echo a") }), "fallback");
    expect(readSkill(join(d, "zipped"), true)).toMatchObject({ name: "zipped", scripts: ["scripts/a.sh"] });
    importSkillZip(d, zipSync({ "SKILL.md": strToU8("---\ndescription: no name\n---\n") }), "flat-one");
    expect(existsSync(join(d, "flat-one", "SKILL.md"))).toBe(true);
  });

  test("a zip entry that escapes is refused", () => {
    const d = scratch();
    const zip = zipSync({ "evil/SKILL.md": strToU8("---\nname: evil\ndescription: x\n---\n"), "evil/../../escape.txt": strToU8("x") });
    expect(() => importSkillZip(d, zip, "evil")).toThrow();
    expect(existsSync(join(d, "..", "escape.txt"))).toBe(false);
  });
});
