import { describe, expect, test } from "bun:test";
import { unwrapData } from "./envelope";

describe("unwrapData", () => {
  test("unwraps Infernet's { data } envelope", () => {
    expect(unwrapData({ data: { admin: true, total_spendable_usd: "12.6" } })).toEqual({ admin: true, total_spendable_usd: "12.6" });
  });
  test("passes a flat body through and rejects non-objects", () => {
    expect(unwrapData({ total_spendable_usd: "1" })).toEqual({ total_spendable_usd: "1" });
    expect(unwrapData(null)).toBeNull();
    expect(unwrapData("x")).toBeNull();
  });
});
