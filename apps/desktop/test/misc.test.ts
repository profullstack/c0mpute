import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readCreds } from "../src/main/account";
import { cliError, cliOutput, locateCli, parseJsonOutput, planShim, shimScript } from "../src/main/cli";
import { matchMediaPoll, summarize } from "../src/main/jobs";
import { gatewayUrl, tailFile, workerStartArgs } from "../src/main/node";
import { isNewer, pickAsset } from "../src/main/updater";
import { interpreterFor } from "../src/main/agent-tools";

const dirs: string[] = [];
const scratch = () => {
  const d = mkdtempSync(join(tmpdir(), "c0-misc-"));
  dirs.push(d);
  return d;
};
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

describe("CLI output", () => {
  test("parses status --json, also after a log line", () => {
    expect(parseJsonOutput<{ a: number }>('{"a":1}\n')).toEqual({ a: 1 });
    expect(parseJsonOutput<{ a: number }>('2026-11-10 INFO something\n{"a":2}')).toEqual({ a: 2 });
    expect(() => parseJsonOutput("worker: running")).toThrow("no JSON");
  });

  test("CLI errors without tracing noise", () => {
    const stderr = "\x1b[2m2026-11-11T00:24:33.287301Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[2mc0mpute_core\x1b[0m: tracing initialised\nError: GET http://127.0.0.1:7997/hosting/v1/sites\n";
    expect(cliError({ code: 1, stdout: "", stderr })).toBe("Error: GET http://127.0.0.1:7997/hosting/v1/sites");
    expect(cliOutput({ code: 0, stdout: "saved to /h/.c0mpute/name\n", stderr })).toBe("saved to /h/.c0mpute/name\nError: GET http://127.0.0.1:7997/hosting/v1/sites");
  });

  test("gateway url from the bind address", () => {
    expect(gatewayUrl("0.0.0.0:7777")).toBe("http://127.0.0.1:7777");
    expect(gatewayUrl("127.0.0.1:7997")).toBe("http://127.0.0.1:7997");
    expect(gatewayUrl("[::]:7777")).toBe("http://[::1]:7777");
    expect(gatewayUrl("10.0.0.5:8000")).toBe("http://10.0.0.5:8000");
  });

  test("worker start flags", () => {
    expect(workerStartArgs({})).toEqual(["worker", "start", "-d"]);
    expect(workerStartArgs({ roles: "tts, stt,;rm -rf", noStorage: true, noHosting: true, private: true })).toEqual([
      "worker", "start", "-d", "--roles", "tts,stt", "--no-storage", "--no-hosting", "--private",
    ]);
  });

  test("tailFile drops the partial first line", () => {
    const f = join(scratch(), "w.log");
    writeFileSync(f, `${"x".repeat(100)}\nline2\nline3\n`);
    expect(tailFile(f, 12)).toEqual(["line3"]);
    expect(tailFile(f)).toEqual(["x".repeat(100), "line2", "line3"]);
    expect(tailFile(join(scratch(), "none"))).toEqual([]);
  });
});

describe("CLI location and shim", () => {
  test("prefers $C0MPUTE_BIN, then bundled, then ~/.c0mpute/bin, then PATH", () => {
    const d = scratch();
    const res = join(d, "res");
    const bin = join(d, "bin");
    for (const p of [join(res, "cli"), join(d, "home", ".c0mpute", "bin"), bin]) mkdirSync(p, { recursive: true });
    writeFileSync(join(bin, "c0mpute"), "", { mode: 0o755 });
    const env = { HOME: join(d, "home"), PATH: bin };
    expect(locateCli(res, env)).toEqual({ path: join(bin, "c0mpute"), source: "path" });
    writeFileSync(join(d, "home", ".c0mpute", "bin", "c0mpute"), "", { mode: 0o755 });
    expect(locateCli(res, env).source).toBe("installed");
    writeFileSync(join(res, "cli", "c0mpute"), "", { mode: 0o755 });
    expect(locateCli(res, env).source).toBe("bundled");
    expect(locateCli(res, { ...env, C0MPUTE_BIN: join(bin, "c0mpute") }).source).toBe("env");
    expect(locateCli(join(d, "nope"), { HOME: join(d, "x"), PATH: "" })).toEqual({ path: null, source: "missing" });
  });

  test("shim plan never replaces another install; AppImage copies the binary", () => {
    expect(planShim({ home: "/h", bundled: "/opt/c0mpute/resources/cli/c0mpute", existing: "/h/.c0mpute/bin/c0mpute", existingIsShim: false }).action).toBe("skip");
    expect(planShim({ home: "/h", bundled: "/opt/c/cli/c0mpute", existing: null, existingIsShim: false })).toEqual({ action: "write", shim: "/h/.local/bin/c0mpute", target: "/opt/c/cli/c0mpute" });
    const ai = planShim({ home: "/h", bundled: "/tmp/.mount_x/resources/cli/c0mpute", appImage: "/h/Apps/c0mpute.AppImage", existing: "/h/.local/bin/c0mpute", existingIsShim: true });
    expect(ai).toMatchObject({ action: "write", copyTo: "/h/.c0mpute/desktop/bin/c0mpute", target: "/h/.c0mpute/desktop/bin/c0mpute" });
    expect(shimScript('/a "b"/c0mpute')).toContain('exec "/a \\"b\\"/c0mpute" "$@"');
  });
});

describe("account", () => {
  test("chat.json key, env overrides", () => {
    const f = join(scratch(), "chat.json");
    writeFileSync(f, JSON.stringify({ url: "https://example.test/", key: "test-key-value" }));
    expect(readCreds(f, {})).toEqual({ url: "https://example.test", key: "test-key-value", source: "chat.json" });
    expect(readCreds(f, { C0MPUTE_API_KEY: "envkey" })).toMatchObject({ key: "envkey", source: "env" });
    expect(readCreds(join(scratch(), "none.json"), {})).toEqual({ url: "https://c0mpute.com", source: "none" });
  });
});

describe("media jobs from the chat", () => {
  test("poll URLs", () => {
    expect(matchMediaPoll("https://c0mpute.com/api/v1/promo/11111111-1111-4111-8111-111111111111")).toEqual({ kind: "promo", id: "11111111-1111-4111-8111-111111111111" });
    expect(matchMediaPoll("https://c0mpute.com/api/v1/images/abcdef12")?.kind).toBe("images");
    expect(matchMediaPoll("https://c0mpute.com/api/v1/promo/abc/video")).toBeNull();
    expect(matchMediaPoll("https://c0mpute.com/api/v1/me")).toBeNull();
    expect(matchMediaPoll("nope")).toBeNull();
  });

  test("status from poll responses", () => {
    expect(summarize(200, "image/png", {})).toEqual({ status: "done", progress: 1 });
    expect(summarize(202, "application/json", { id: "x", status: "pending", stage: "queued", position: 3 })).toMatchObject({ status: "queued", detail: "queued (#3 in line)" });
    const promo = summarize(202, "application/json", { status: "pending", stages: { script: { state: "done" }, voice: { state: "running" }, render: { state: "waiting" }, mux: { state: "waiting" } } });
    expect(promo).toMatchObject({ status: "running", progress: 0.25, detail: "voice" });
    expect(summarize(502, "application/json", { error: { message: "song failed" } })).toEqual({ status: "failed", detail: "song failed" });
    expect(summarize(200, "application/json", { status: "done", video: "/v" })).toMatchObject({ status: "done", progress: 1 });
  });
});

describe("updater", () => {
  test("version order", () => {
    expect(isNewer("v0.3.29", "0.3.28")).toBe(true);
    expect(isNewer("0.10.0", "0.9.9")).toBe(true);
    expect(isNewer("0.3.28", "0.3.28")).toBe(false);
    expect(isNewer("0.3.27", "0.3.28")).toBe(false);
  });

  test("picks the AppImage for this arch", () => {
    const rel = {
      tag_name: "v0.3.29",
      html_url: "",
      assets: [
        { name: "c0mpute-linux-x86_64.tar.gz", browser_download_url: "a" },
        { name: "c0mpute-desktop-0.3.29-linux-x86_64.AppImage", browser_download_url: "b" },
        { name: "c0mpute-desktop-0.3.29-linux-arm64.AppImage", browser_download_url: "c" },
        { name: "c0mpute-desktop-0.3.29-linux-amd64.deb", browser_download_url: "d" },
      ],
    };
    expect(pickAsset(rel, "linux", "x64", "AppImage")?.browser_download_url).toBe("b");
    expect(pickAsset(rel, "linux", "arm64", "AppImage")?.browser_download_url).toBe("c");
    expect(pickAsset(rel, "linux", "x64", "deb")?.browser_download_url).toBe("d");
  });
});

describe("skill scripts", () => {
  test("interpreter by extension", () => {
    expect(interpreterFor("/s/scripts/a.sh")).toEqual({ cmd: "bash", args: ["/s/scripts/a.sh"] });
    expect(interpreterFor("/s/scripts/a.py").cmd).toBe("python3");
    expect(interpreterFor("/s/scripts/run")).toEqual({ cmd: "/s/scripts/run", args: [] });
  });
});
