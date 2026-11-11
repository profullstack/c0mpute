import { describe, expect, test } from "bun:test";
import { describeAlbumStage, parseAlbumCommand, parseAlbumRequest, sanitizeAlbum, trackFileBase } from "./album";

const ID = "0f8fad5b-d9cb-469f-a165-70867728950e";

describe("chat album", () => {
  test("parses /album and /lp", () => {
    expect(parseAlbumCommand("/album dark synthwave about a drowned city")).toEqual({ prompt: "dark synthwave about a drowned city" });
    expect(parseAlbumCommand(" /LP folk ")).toEqual({ prompt: "folk" });
    expect(parseAlbumCommand("/album")).toBeNull();
    expect(parseAlbumCommand("/albums x")).toBeNull();
    expect(parseAlbumCommand("/music x")).toBeNull();
  });

  test("validates the request", () => {
    expect(parseAlbumRequest({ prompt: " punk " })).toEqual({ prompt: "punk" });
    expect(parseAlbumRequest({ prompt: "punk", tracks: 10, duration: 90 })).toEqual({ prompt: "punk", tracks: 10, duration: 90 });
    expect(typeof parseAlbumRequest({ prompt: "punk", tracks: 1 })).toBe("string");
    expect(typeof parseAlbumRequest({ prompt: "punk", tracks: 13 })).toBe("string");
    expect(typeof parseAlbumRequest({ prompt: "punk", duration: 601 })).toBe("string");
    expect(typeof parseAlbumRequest({ prompt: "" })).toBe("string");
  });

  test("keeps only well-typed upstream fields", () => {
    const a = sanitizeAlbum({
      stage: "recording",
      band: "Glass Harbor",
      album: "Night Ferry",
      cover: "ready",
      evil: "<script>",
      tracks: [
        { n: 1, title: "One", status: "done", id: ID, lyrics: "[verse]\nhey", duration: 120 },
        { n: 2, title: "Two", status: "pending", id: "../../etc", lyrics: "[Instrumental]", node: { stage: "rendering", worker: "abc" } },
        { title: 5, status: "weird" },
      ],
    });
    expect(a).toMatchObject({ stage: "recording", band: "Glass Harbor", album: "Night Ferry", cover: true });
    expect("evil" in a).toBe(false);
    expect(a.tracks[0]).toMatchObject({ n: 1, title: "One", status: "done", id: ID, lyrics: "[verse]\nhey", duration: 120 });
    expect(a.tracks[1]).toMatchObject({ id: null, lyrics: null, node: { stage: "rendering", worker: "abc" } });
    expect(a.tracks[2]).toMatchObject({ n: 3, title: "Track 3", status: "planned" });
    expect(sanitizeAlbum(null)).toMatchObject({ stage: "starting", tracks: [] });
  });

  test("describes progress and names files", () => {
    const a = sanitizeAlbum({
      stage: "recording",
      tracks: [{ status: "done" }, { status: "pending", node: { stage: "rendering" } }, { status: "pending" }, { status: "failed" }],
    });
    expect(describeAlbumStage(a)).toBe("recording on c0mpute workers · 1/4 done · 1 rendering · 1 waiting for a worker · 1 failed");
    expect(describeAlbumStage(sanitizeAlbum({ stage: "planning", tracksWanted: 8 }))).toBe("naming the band, the album and 8 songs");
    expect(trackFileBase(3, 'Neon / Rain: "Live"')).toBe("03 - Neon Rain Live");
  });
});
