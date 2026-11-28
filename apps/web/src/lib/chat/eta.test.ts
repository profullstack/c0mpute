import { describe, expect, test } from "bun:test";
import { clock, liveStatus } from "./eta";

describe("liveStatus", () => {
  const t0 = 1_000_000;
  test("counts the estimate down from when the status arrived", () => {
    expect(liveStatus("running on a node (about 211 s)", t0, t0 - 5_000, t0)).toBe("running on a node (about 3:31 left) · 0:05");
    expect(liveStatus("running on a node (about 211 s)", t0, t0 - 5_000, t0 + 61_000)).toBe("running on a node (about 2:30 left) · 1:06");
  });
  test("past the estimate it says so instead of a negative number", () => {
    expect(liveStatus("running on a node (about 10 s)", t0, t0, t0 + 22_000)).toBe("running on a node (0:12 over the estimate) · 0:22");
  });
  test("minutes in the estimate", () => {
    expect(liveStatus("queued (about 2 min)", t0, t0, t0 + 30_000)).toBe("queued (about 1:30 left) · 0:30");
  });
  test("no estimate: just the time waited", () => {
    expect(liveStatus("asking a node…", t0, t0 - 3_000, t0)).toBe("asking a node… · 0:03");
    expect(liveStatus(null, t0, t0, t0 + 1_000)).toBe("sending it to the network… · 0:01");
  });
  test("clock", () => {
    expect(clock(7)).toBe("0:07");
    expect(clock(3725)).toBe("1:02:05");
  });
});
