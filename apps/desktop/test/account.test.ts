import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { accountStorePath, clearKeychainCreds, forgetChatJsonKey, keyFingerprint, readCreds, type KeychainBackend } from "../src/main/account";

const dirs: string[] = [];
const scratch = () => {
  const d = mkdtempSync(join(tmpdir(), "c0-acct-"));
  dirs.push(d);
  return d;
};
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

/** A stand-in for Electron safeStorage: reversible, and never the plaintext. */
function fakeKeychain(available = true): KeychainBackend & { encrypts: number } {
  const k = {
    encrypts: 0,
    isEncryptionAvailable: () => available,
    encryptString: (s: string) => {
      k.encrypts++;
      return Buffer.from(`fake:${Buffer.from(s).toString("hex").split("").reverse().join("")}`);
    },
    decryptString: (b: Buffer) => {
      const t = b.toString();
      if (!t.startsWith("fake:")) throw new Error("not ours");
      return Buffer.from(t.slice(5).split("").reverse().join(""), "hex").toString();
    },
  };
  return k;
}

function chatJson(dir: string, body: object, mtimeSec?: number) {
  const f = join(dir, "chat.json");
  writeFileSync(f, JSON.stringify(body), { mode: 0o600 });
  if (mtimeSec !== undefined) utimesSync(f, mtimeSec, mtimeSec);
  return f;
}

describe("account key in the OS keychain", () => {
  test("first read imports chat.json; the store never holds the plaintext", () => {
    const d = scratch();
    const f = chatJson(d, { url: "https://example.test/", key: "secret-key-AAA" }, 1_700_000_000);
    const kc = fakeKeychain();
    expect(readCreds(f, {}, { backend: kc })).toEqual({ url: "https://example.test", key: "secret-key-AAA", source: "keychain", keychain: true });
    const store = accountStorePath(f);
    expect(store).toBe(join(d, "desktop-account.json"));
    const raw = readFileSync(store, "utf8");
    expect(raw).not.toContain("secret-key-AAA");
    const j = JSON.parse(raw);
    expect(j).toMatchObject({ url: "https://example.test", fingerprint: keyFingerprint("secret-key-AAA"), imported_from_mtime_ms: statSync(f).mtimeMs });
    expect(typeof j.enc).toBe("string");
    expect(statSync(store).mode & 0o777).toBe(0o600);
    expect(existsSync(`${store}.tmp`)).toBe(false);
  });

  test("later reads come from the keychain, even with chat.json gone", () => {
    const d = scratch();
    const f = chatJson(d, { url: "https://example.test", key: "secret-key-AAA" });
    const kc = fakeKeychain();
    readCreds(f, {}, { backend: kc });
    rmSync(f);
    expect(readCreds(f, {}, { backend: kc })).toEqual({ url: "https://example.test", key: "secret-key-AAA", source: "keychain", keychain: true });
    expect(kc.encrypts).toBe(1);
  });

  test("re-imports when chat.json is rewritten with another key, not when unchanged", () => {
    const d = scratch();
    const f = chatJson(d, { key: "secret-key-AAA" }, 1_700_000_000);
    const kc = fakeKeychain();
    readCreds(f, {}, { backend: kc });
    readCreds(f, {}, { backend: kc });
    expect(kc.encrypts).toBe(1);

    // Same key rewritten later (the CLI touched the file): nothing to do.
    chatJson(d, { key: "secret-key-AAA" }, 1_700_000_100);
    expect(readCreds(f, {}, { backend: kc }).key).toBe("secret-key-AAA");
    expect(kc.encrypts).toBe(1);

    // `c0mpute login` again as another account.
    chatJson(d, { url: "https://other.test", key: "secret-key-BBB" }, 1_700_000_200);
    expect(readCreds(f, {}, { backend: kc })).toEqual({ url: "https://other.test", key: "secret-key-BBB", source: "keychain", keychain: true });
    expect(kc.encrypts).toBe(2);
    expect(JSON.parse(readFileSync(accountStorePath(f), "utf8")).fingerprint).toBe(keyFingerprint("secret-key-BBB"));

    // An older chat.json with a different key does not override the store.
    chatJson(d, { key: "secret-key-CCC" }, 1_600_000_000);
    expect(readCreds(f, {}, { backend: kc }).key).toBe("secret-key-BBB");
  });

  test("env still wins", () => {
    const d = scratch();
    const f = chatJson(d, { url: "https://example.test", key: "secret-key-AAA" });
    const kc = fakeKeychain();
    expect(readCreds(f, { C0MPUTE_API_KEY: "envkey", C0MPUTE_URL: "https://env.test/" }, { backend: kc })).toEqual({ url: "https://env.test", key: "envkey", source: "env", keychain: true });
    // The import still happened, so the keychain is ready without the override.
    expect(readCreds(f, {}, { backend: kc }).source).toBe("keychain");
  });

  test("no keyring: falls back to chat.json and writes nothing", () => {
    const d = scratch();
    const f = chatJson(d, { url: "https://example.test", key: "secret-key-AAA" });
    for (const backend of [fakeKeychain(false), null]) {
      expect(readCreds(f, {}, { backend })).toEqual({ url: "https://example.test", key: "secret-key-AAA", source: "chat.json", keychain: false });
    }
    const throwing: KeychainBackend = {
      isEncryptionAvailable: () => {
        throw new Error("no dbus");
      },
      encryptString: () => Buffer.alloc(0),
      decryptString: () => "",
    };
    expect(readCreds(f, {}, { backend: throwing })).toMatchObject({ source: "chat.json", keychain: false });
    expect(existsSync(accountStorePath(f))).toBe(false);
  });

  test("encryption failing mid-import falls back to chat.json", () => {
    const d = scratch();
    const f = chatJson(d, { key: "secret-key-AAA" });
    const kc = fakeKeychain();
    kc.encryptString = () => {
      throw new Error("keyring locked");
    };
    expect(readCreds(f, {}, { backend: kc })).toMatchObject({ key: "secret-key-AAA", source: "chat.json", keychain: true });
    expect(existsSync(accountStorePath(f))).toBe(false);
  });

  test("corrupt store: falls back and imports again", () => {
    const d = scratch();
    const f = chatJson(d, { key: "secret-key-AAA" });
    const kc = fakeKeychain();
    for (const junk of ["{not json", JSON.stringify({ enc: Buffer.from("garbage").toString("base64"), fingerprint: "x", imported_from_mtime_ms: Date.now() + 1e9 })]) {
      writeFileSync(accountStorePath(f), junk);
      expect(readCreds(f, {}, { backend: kc })).toMatchObject({ key: "secret-key-AAA", source: "keychain" });
      expect(JSON.parse(readFileSync(accountStorePath(f), "utf8")).fingerprint).toBe(keyFingerprint("secret-key-AAA"));
    }
    // Corrupt with no chat.json to import from: signed out, no crash.
    writeFileSync(accountStorePath(f), "{not json");
    rmSync(f);
    expect(readCreds(f, {}, { backend: kc })).toEqual({ url: "https://c0mpute.com", source: "none", keychain: true });
  });

  test("sign-out clears the store and the chat.json key", () => {
    const d = scratch();
    const f = chatJson(d, { url: "https://example.test", key: "secret-key-AAA" });
    const kc = fakeKeychain();
    readCreds(f, {}, { backend: kc });
    clearKeychainCreds(f);
    expect(existsSync(accountStorePath(f))).toBe(false);
    expect(forgetChatJsonKey(f)).toBe(true);
    expect(JSON.parse(readFileSync(f, "utf8"))).toEqual({ url: "https://example.test" });
    expect(forgetChatJsonKey(f)).toBe(false);
    expect(readCreds(f, {}, { backend: kc })).toEqual({ url: "https://example.test", source: "none", keychain: true });
    expect(existsSync(accountStorePath(f))).toBe(false);
    // Idempotent, and fine when nothing is there.
    clearKeychainCreds(f);
    rmSync(f);
    expect(forgetChatJsonKey(f)).toBe(false);
  });
});
