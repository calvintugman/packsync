import { describe, expect, it } from "vitest";
import { href, parseRoute } from "./router";

describe("routes", () => {
  it("parses the app's addresses", () => {
    expect(parseRoute("")).toEqual({ name: "instances" });
    expect(parseRoute("#/")).toEqual({ name: "instances" });
    expect(parseRoute("#/cloud")).toEqual({ name: "cloud" });
    expect(parseRoute("#/activity")).toEqual({ name: "activity" });
    expect(parseRoute("#/settings")).toEqual({ name: "settings" });
    expect(parseRoute("#/setup")).toEqual({ name: "setup" });
    expect(parseRoute("#/instance/abc")).toEqual({ name: "instance", id: "abc", tab: null });
    expect(parseRoute("#/instance/abc/hooks")).toEqual({ name: "instance", id: "abc", tab: "hooks" });
  });
  it("falls back to the instance list for anything unknown", () => {
    expect(parseRoute("#/nonsense")).toEqual({ name: "instances" });
    expect(parseRoute("#/instance")).toEqual({ name: "instances" });
  });
  it("ignores the bridge token that may still be in the address", () => {
    expect(parseRoute("#token=abcdef0123")).toEqual({ name: "instances" });
  });
  it("round-trips", () => {
    for (const r of [
      { name: "instances" as const },
      { name: "cloud" as const },
      { name: "instance" as const, id: "a b", tab: "history" },
      { name: "instance" as const, id: "x", tab: null },
    ]) {
      expect(parseRoute(href(r))).toEqual(r);
    }
  });
});
