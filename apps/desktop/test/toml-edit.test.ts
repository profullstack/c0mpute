import { describe, expect, test } from "bun:test";
import { parse } from "smol-toml";
import { applyNodeSettings, checkConfigText, readNodeSettings, setTomlValue, unsetTomlValue } from "../src/main/toml-edit";

const CONFIG = `# my node
roles = ["storage", "tts"]
update_auto = true

[gateway]
bind = "0.0.0.0:7777" # public
auto_announce = true

[storage]
# keep the disk
root = "/mnt/data/c0mpute"
enabled = true

[payments]
enabled = false
rail = "coinpay"

[payments.coinpay]
api_key_env = "COINPAY_API_KEY"
`;

describe("config.toml line edits", () => {
  test("replaces a value in place and keeps every comment", () => {
    const out = setTomlValue(CONFIG, "storage", "enabled", false);
    expect(out).toContain("# my node");
    expect(out).toContain("# keep the disk");
    expect(out).toContain('bind = "0.0.0.0:7777" # public');
    expect((parse(out) as any).storage.enabled).toBe(false);
    expect(out.split("\n").length).toBe(CONFIG.split("\n").length);
  });

  test("inserts a missing key at the end of its section", () => {
    const out = setTomlValue(CONFIG, "gateway", "announce", "http://1.2.3.4:7777");
    const v = parse(out) as any;
    expect(v.gateway.announce).toBe("http://1.2.3.4:7777");
    expect(out.indexOf("announce = ")).toBeLessThan(out.indexOf("[storage]"));
  });

  test("adds a missing section", () => {
    const out = setTomlValue(CONFIG, "hosting", "enabled", false);
    expect((parse(out) as any).hosting.enabled).toBe(false);
  });

  test("top-level keys go before the first section", () => {
    const out = setTomlValue(CONFIG, null, "update_auto", false);
    expect((parse(out) as any).update_auto).toBe(false);
    const fresh = setTomlValue("", null, "update_auto", false);
    expect(parse(fresh)).toEqual({ update_auto: false });
  });

  test("replaces a multi-line array whole", () => {
    const t = 'roles = [\n  "a",\n  "b",\n]\n[x]\ny = 1\n';
    const out = setTomlValue(t, null, "roles", ["c"]);
    expect(parse(out)).toEqual({ roles: ["c"], x: { y: 1 } });
  });

  test("dotted section headers are matched exactly", () => {
    const out = setTomlValue(CONFIG, "payments.coinpay", "api_key_env", "MY_KEY");
    const v = parse(out) as any;
    expect(v.payments.coinpay.api_key_env).toBe("MY_KEY");
    expect(v.payments.enabled).toBe(false);
  });

  test("unset removes only that key", () => {
    const out = unsetTomlValue(CONFIG, "gateway", "auto_announce");
    expect((parse(out) as any).gateway).toEqual({ bind: "0.0.0.0:7777" });
  });

  test("strings are escaped", () => {
    const out = setTomlValue("", "hosting", "payout_wallet", 'a"b\\c');
    expect((parse(out) as any).hosting.payout_wallet).toBe('a"b\\c');
  });
});

describe("secrets never go in config.toml", () => {
  test("a key under [payments] is refused", () => {
    const bad = `${CONFIG}\n[payments.coinpay.extra]\napi_key = "plain-value-here"\n`;
    expect(checkConfigText(bad).ok).toBe(false);
    expect(checkConfigText(`[payments]\napi_key = "x"\n`).error).toContain("api_key_env");
  });

  test("api_key_env must be a variable name, not the key", () => {
    expect(checkConfigText(`[payments.coinpay]\napi_key_env = "looks-like-a-key-value"\n`).ok).toBe(false);
    expect(() => applyNodeSettings(CONFIG, { apiKeyEnv: "looks-like-a-key-value" })).toThrow();
    expect(checkConfigText(CONFIG).ok).toBe(true);
  });

  test("only coinpay is a rail", () => {
    expect(() => applyNodeSettings(CONFIG, { paymentsRail: "stripe" })).toThrow();
  });

  test("broken TOML is refused", () => {
    expect(checkConfigText("[storage\nenabled = true").ok).toBe(false);
  });
});

describe("node settings form", () => {
  test("reads defaults from an empty config", () => {
    const s = readNodeSettings({});
    expect(s).toMatchObject({ storage: true, hosting: true, private: false, autoUpdate: true, paymentsEnabled: false, apiKeyEnv: "C0MPUTE_COINPAY_PAYOUT_KEY" });
  });

  test("round trip through the form", () => {
    const out = applyNodeSettings(CONFIG, {
      storage: false,
      hosting: false,
      private: true,
      paymentsEnabled: true,
      paymentsRail: "coinpay",
      apiKeyEnv: "C0_COINPAY_KEY",
      recipientEmail: "ops@example.com",
      gatewayBind: "127.0.0.1:7997",
    });
    const s = readNodeSettings(parse(out) as any);
    expect(s).toMatchObject({ storage: false, hosting: false, private: true, paymentsEnabled: true, apiKeyEnv: "C0_COINPAY_KEY", recipientEmail: "ops@example.com", gatewayBind: "127.0.0.1:7997" });
    expect(out).toContain("# keep the disk");
  });

  test("bad gateway bind is refused", () => {
    expect(() => applyNodeSettings(CONFIG, { gatewayBind: "not a bind" })).toThrow();
  });
});
