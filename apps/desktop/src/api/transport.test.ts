import { describe, expect, it, vi } from "vitest";
import { ApiError } from "./errors";
import { createTransport, HttpTransport, readBridgeToken } from "./transport";
import type { AppEvent } from "./types";

function memStorage(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v) };
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

describe("readBridgeToken", () => {
  it("takes the token from the address and remembers it for the tab", () => {
    const s = memStorage();
    expect(readBridgeToken("#token=abc123", s)).toBe("abc123");
    expect(readBridgeToken("#/instances", s)).toBe("abc123");
  });
  it("finds the token among other fragment parts", () => {
    expect(readBridgeToken("#/cloud&token=0f0f", memStorage())).toBe("0f0f");
  });
  it("has none when neither the address nor the tab does", () => {
    expect(readBridgeToken("#/", memStorage())).toBeNull();
    expect(readBridgeToken("", null)).toBeNull();
  });
  it("survives blocked storage", () => {
    const blocked = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); } };
    expect(readBridgeToken("#token=beef", blocked)).toBe("beef");
    expect(readBridgeToken("#/", blocked)).toBeNull();
  });
});

describe("HttpTransport", () => {
  it("sends the token and returns the result", async () => {
    const f = vi.fn().mockResolvedValue(jsonResponse({ ok: true, result: { x: 1 } }));
    const t = new HttpTransport("tok", "", f as unknown as typeof fetch);
    await expect(t.call("app.info")).resolves.toEqual({ x: 1 });
    const [url, init] = f.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/call");
    expect((init.headers as Record<string, string>)["x-packsync-token"]).toBe("tok");
    expect(JSON.parse(init.body as string)).toEqual({ method: "app.info", params: null });
  });

  it("starts jobs on their own endpoint", async () => {
    const f = vi.fn().mockResolvedValue(jsonResponse({ ok: true, result: { job: "j1" } }));
    const t = new HttpTransport("tok", "", f as unknown as typeof fetch);
    await expect(t.startJob("instance.sync", { instance: "i" })).resolves.toBe("j1");
    expect((f.mock.calls[0] as [string])[0]).toBe("/api/job");
  });

  it("turns the engine's error into an ApiError", async () => {
    const f = vi.fn().mockResolvedValue(jsonResponse({ ok: false, error: { code: "conflict", message: "two versions", advice: "pick", exit_code: 12 } }));
    const t = new HttpTransport("tok", "", f as unknown as typeof fetch);
    const err = await t.call("instance.sync", {}).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toMatchObject({ code: "conflict", exitCode: 12 });
  });

  it("explains an unauthorised or unreachable bridge", async () => {
    const t401 = new HttpTransport("bad", "", vi.fn().mockResolvedValue(jsonResponse({ ok: false }, 401)) as unknown as typeof fetch);
    await expect(t401.call("app.info")).rejects.toMatchObject({ code: "bridge_unauthorized" });
    const tDown = new HttpTransport("t", "", vi.fn().mockRejectedValue(new TypeError("fetch failed")) as unknown as typeof fetch);
    await expect(tDown.call("app.info")).rejects.toMatchObject({ code: "bridge_unreachable" });
    const tJunk = new HttpTransport("t", "", vi.fn().mockResolvedValue(new Response("<html>", { status: 502 })) as unknown as typeof fetch);
    await expect(tJunk.call("app.info")).rejects.toMatchObject({ code: "internal" });
  });

  it("streams events and reports the connection", () => {
    const sources: { onopen?: () => void; onerror?: () => void; onmessage?: (m: MessageEvent<string>) => void; close: () => void; url: string }[] = [];
    const t = new HttpTransport("tok", "", fetch, (url) => {
      const s = { url, close: vi.fn() };
      sources.push(s);
      return s as unknown as EventSource;
    });
    const events: AppEvent[] = [];
    const status: string[] = [];
    const stop = t.subscribe((e) => events.push(e), (s) => status.push(s));
    const es = sources[0]!;
    expect(es.url).toBe("/api/events?token=tok");
    es.onopen?.();
    es.onmessage?.({ data: JSON.stringify({ type: "storage_changed" }) } as MessageEvent<string>);
    es.onmessage?.({ data: "not json" } as MessageEvent<string>);
    es.onerror?.();
    expect(status).toEqual(["connecting", "live", "lost"]);
    expect(events).toEqual([{ type: "storage_changed" }]);
    stop();
    expect(es.close).toHaveBeenCalled();
  });
});

describe("createTransport", () => {
  it("uses the bridge when the address carries a token", () => {
    const w = { location: { hash: "#token=ab12" }, sessionStorage: memStorage() } as unknown as Window;
    expect(createTransport(w)?.kind).toBe("http");
  });
  it("uses Tauri when the shell injected its bridge", () => {
    const w = { __TAURI_INTERNALS__: {}, location: { hash: "" } } as unknown as Window;
    expect(createTransport(w)?.kind).toBe("tauri");
  });
  it("has nothing to talk to in a plain browser", () => {
    const w = { location: { hash: "" }, sessionStorage: memStorage() } as unknown as Window;
    expect(createTransport(w)).toBeNull();
  });
});
