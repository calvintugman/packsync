import { describe, expect, it } from "vitest";
import { baseName, clamp01, formatBytes, formatDuration, formatRelative, packLine, plural, worldName, worldsLine } from "./format";

describe("formatBytes", () => {
  it("uses decimal units like the CLI and Google Drive", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(999)).toBe("999 B");
    expect(formatBytes(1000)).toBe("1.0 KB");
    expect(formatBytes(1_500_000)).toBe("1.5 MB");
    expect(formatBytes(12_345_678_901)).toBe("12.3 GB");
    expect(formatBytes(250_000_000)).toBe("250 MB");
  });
  it("never invents a number", () => {
    expect(formatBytes(null)).toBe("–");
    expect(formatBytes(undefined)).toBe("–");
    expect(formatBytes(Number.NaN)).toBe("–");
    expect(formatBytes(-1)).toBe("–");
  });
});

describe("plural", () => {
  it("counts and pluralises", () => {
    expect(plural(1, "file")).toBe("1 file");
    expect(plural(0, "file")).toBe("0 files");
    expect(plural(1234, "file")).toBe("1,234 files");
    expect(plural(2, "index object", "index objects")).toBe("2 index objects");
  });
});

describe("formatRelative", () => {
  const now = Date.parse("2026-03-10T12:00:00Z");
  it("speaks in the units a person uses", () => {
    expect(formatRelative(now - 10_000, now)).toBe("just now");
    expect(formatRelative(now - 5 * 60_000, now)).toBe("5 minutes ago");
    expect(formatRelative(now - 3 * 3600_000, now)).toBe("3 hours ago");
    expect(formatRelative(now - 86400_000, now)).toBe("yesterday");
    expect(formatRelative(now + 2 * 60_000, now)).toBe("in 2 minutes");
  });
  it("accepts ISO strings and falls back to a date after a month", () => {
    expect(formatRelative(new Date(now - 7200_000).toISOString(), now)).toBe("2 hours ago");
    expect(formatRelative(now - 90 * 86400_000, now)).toMatch(/2025|Dec/);
  });
  it("says 'never' for nothing", () => {
    expect(formatRelative(null, now)).toBe("never");
    expect(formatRelative("not a date", now)).toBe("never");
  });
});

describe("pack and world lines", () => {
  it("names the loader properly", () => {
    expect(packLine({ minecraft_version: "1.20.1", loader: { kind: "neoforge", version: "21.1" }, components: [] })).toBe("Minecraft 1.20.1 · NeoForge 21.1");
    expect(packLine({ minecraft_version: "1.21", loader: null, components: [] })).toBe("Minecraft 1.21");
    expect(packLine({ minecraft_version: null, loader: null, components: [] })).toBe("Unknown version");
    expect(packLine({ minecraft_version: "1.12.2", loader: { kind: "futureloader", version: "1" }, components: [] })).toBe("Minecraft 1.12.2 · futureloader 1");
  });
  it("prefers the world's own name", () => {
    expect(worldName({ folder: "New World", name: " Home ", last_played_ms: null })).toBe("Home");
    expect(worldName({ folder: "New World", name: null, last_played_ms: null })).toBe("New World");
    expect(worldsLine([])).toBe("No worlds yet");
    expect(worldsLine([{ folder: "a", name: "Ünï", last_played_ms: null }])).toBe("World “Ünï”");
    expect(worldsLine([{ folder: "a", name: null, last_played_ms: null }, { folder: "b", name: null, last_played_ms: null }])).toBe("2 worlds");
  });
});

describe("small helpers", () => {
  it("formats durations", () => {
    expect(formatDuration(900)).toBe("1 s");
    expect(formatDuration(75_000)).toBe("1 min 15 s");
    expect(formatDuration(3_900_000)).toBe("1 h 5 min");
  });
  it("takes the last path component of either separator", () => {
    expect(baseName("a/b/c.txt")).toBe("c.txt");
    expect(baseName("C:\\Users\\me\\Pack")).toBe("Pack");
    expect(baseName("/")).toBe("/");
  });
  it("clamps", () => {
    expect(clamp01(-1)).toBe(0);
    expect(clamp01(2)).toBe(1);
    expect(clamp01(0.4)).toBe(0.4);
  });
});
