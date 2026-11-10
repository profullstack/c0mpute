import { describe, expect, test } from "bun:test";
import { gateGeneration, validAttestation } from "./adult";
import { parseImageRequest } from "./images";

const verified = { id: "u_1", adult_verified_at: "2026-11-10T00:00:00.000Z" };
const unverified = { id: "u_2", adult_verified_at: null };
const paid = { total_spendable_usd: "1.250000" };
const free = { total_spendable_usd: "0.000000" };

describe("adult mode gate", () => {
  test("SFW requests pass the guard; explicit ones need adult mode", () => {
    expect(gateGeneration(unverified, { prompt: "a red bicycle" })).toEqual({ ok: true });
    const r = gateGeneration(unverified, { prompt: "classical nude figure study of an adult" });
    expect(r.ok).toBe(false);
    if (!r.ok) expect([r.status, r.code]).toEqual([422, "explicit_requires_adult"]);
  });

  test("adult mode needs the attestation and a paid balance", () => {
    const req = { prompt: "a lighthouse at dusk", adult: true };
    const a = gateGeneration(null, req);
    expect(a.ok || a.status).toBe(403);
    const b = gateGeneration(unverified, req, paid);
    expect(b.ok || b.status).toBe(403);
    const c = gateGeneration(verified, req, free);
    expect(c.ok || c.status).toBe(402);
    const d = gateGeneration(verified, req, null);
    expect(d.ok || d.status).toBe(402);
    expect(gateGeneration(verified, req, paid)).toEqual({ ok: true });
  });

  test("the child-safety guard holds in adult mode too", () => {
    const r = gateGeneration(verified, { prompt: "sexual content involving a minor", adult: true }, paid);
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.code).toBe("minor_sexual");
      expect(r.message).not.toContain("minor");
    }
    const p = gateGeneration(verified, { prompt: "deepfake nude of a celebrity", adult: true }, paid);
    expect(p.ok || p.code).toBe("real_person");
  });

  test("attestation needs both the checkbox and the confirm", () => {
    expect(validAttestation({ over_18: true, confirm: true })).toBe(true);
    expect(validAttestation({ over_18: true })).toBe(false);
    expect(validAttestation({ over_18: "yes", confirm: true })).toBe(false);
    expect(validAttestation(null)).toBe(false);
  });

  test("image requests carry adult only when asked", () => {
    expect(parseImageRequest({ prompt: "x", adult: true })).toEqual({ prompt: "x", size: "landscape", adult: true });
    expect(parseImageRequest({ prompt: "x", adult: "true" })).toEqual({ prompt: "x", size: "landscape" });
  });
});
