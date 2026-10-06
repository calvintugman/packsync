import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../api/errors";
import type { JobInfo } from "../api/types";
import { baseTransport, instance, withPhase } from "../test/fixtures";
import { AppStore } from "./store";

async function started(t = baseTransport()) {
  const store = new AppStore(t, { pollMs: 0 });
  await store.start();
  return { store, t };
}

afterEach(() => vi.useRealTimers());

describe("AppStore: loading", () => {
  it("loads settings, storage, instances and jobs on start", async () => {
    const { store } = await started();
    const s = store.getState();
    expect(s.status).toBe("ready");
    expect(s.instances.map((i) => i.name)).toEqual(["Skyblock Pack"]);
    expect(s.storage?.connected).toBe(true);
    expect(s.settings?.device_name).toBe("Desktop");
    expect(s.info?.version).toBe("0.1.0");
    store.stop();
  });

  it("reports why it could not start instead of showing an empty app", async () => {
    const t = baseTransport();
    t.on("app.info", () => {
      throw new ApiError({ code: "bridge_unreachable", message: "no bridge", advice: "start it" });
    });
    const { store } = await started(t);
    expect(store.getState().status).toBe("failed");
    expect(store.getState().loadError).toMatchObject({ code: "bridge_unreachable", advice: "start it" });
    store.stop();
  });

  it("starts only once", async () => {
    const { store, t } = await started();
    await store.start();
    expect(t.count("app.info")).toBe(1);
    store.stop();
  });
});

describe("AppStore: jobs", () => {
  it("resolves with the job's result", async () => {
    const t = baseTransport();
    t.on("instance.sync", () => ({ ok: 1 }));
    const { store } = await started(t);
    await expect(store.runJob("instance.sync", { instance: "i" })).resolves.toEqual({ ok: 1 });
    store.stop();
  });

  it("rejects with the engine's error when the job fails", async () => {
    const t = baseTransport();
    t.on("instance.sync", () => {
      throw new ApiError({ code: "offline", message: "no network", advice: "try later", exit_code: 10 });
    });
    const { store } = await started(t);
    await expect(store.runJob("instance.sync", { instance: "i" })).rejects.toMatchObject({ code: "offline", advice: "try later" });
    store.stop();
  });

  it("rejects with 'cancelled' when the job was cancelled", async () => {
    const t = baseTransport();
    t.holdJobs = true;
    const { store } = await started(t);
    const p = store.runJob("instance.sync", { instance: "i" });
    await vi.waitFor(() => expect(t.jobs.size).toBe(1));
    t.finishWith("job-1", { state: "cancelled" });
    await expect(p).rejects.toMatchObject({ code: "cancelled" });
    store.stop();
  });

  it("tells the caller the job id as soon as it exists", async () => {
    const t = baseTransport();
    t.on("instance.sync", () => 1);
    const { store } = await started(t);
    const seen: string[] = [];
    await store.runJob("instance.sync", { instance: "i" }, { onStart: (id) => seen.push(id) });
    expect(seen).toEqual(["job-1"]);
    store.stop();
  });

  it("follows progress while a job runs and forgets it afterwards", async () => {
    const t = baseTransport();
    t.holdJobs = true;
    t.on("instance.upload", () => ({ done: true }));
    const { store } = await started(t);
    const p = store.runJob("instance.upload", { instance: "i" });
    await vi.waitFor(() => expect(t.jobs.size).toBe(1));
    t.emit({
      type: "progress", job: "job-1", instance: "i", phase: "upload", kind: { type: "progress" },
      message: "uploading", bytes_done: 5, bytes_total: 10, items_done: 0, items_total: 0,
    });
    expect(store.getState().progress["job-1"]?.bytes_done).toBe(5);
    expect(store.runningJobs("i")).toHaveLength(1);
    t.finish("job-1");
    await p;
    expect(store.getState().progress["job-1"]).toBeUndefined();
    expect(store.runningJobs("i")).toHaveLength(0);
    store.stop();
  });

  it("does not lose a job that finished while the event stream was down", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const t = baseTransport();
    t.holdJobs = true;
    t.on("cloud.instances", () => []);
    const { store } = await started(t);
    const p = store.runJob("cloud.instances", undefined);
    await vi.waitFor(() => expect(t.jobs.size).toBe(1));
    // the finished event never reaches the UI, but the job list shows it
    const done: JobInfo = { ...t.jobs.get("job-1")!, state: "succeeded", finished_ms: Date.now(), result: ["found"] };
    t.jobs.set("job-1", done);
    await vi.advanceTimersByTimeAsync(2500);
    await expect(p).resolves.toEqual(["found"]);
    store.stop();
  });

  it("keeps only a bounded number of finished jobs", async () => {
    const t = baseTransport();
    t.on("instance.sync", () => 0);
    const store = new AppStore(t, { pollMs: 0, keepJobs: 3 });
    await store.start();
    for (let i = 0; i < 6; i++) {
      await store.runJob("instance.sync", { instance: "i" });
      await new Promise((r) => setTimeout(r, 3)); // distinct finish times: the oldest are the ones dropped
    }
    const kept = Object.values(store.getState().jobs);
    expect(kept.length).toBeLessThanOrEqual(3);
    expect(kept.map((j) => j.id).sort()).toEqual(["job-4", "job-5", "job-6"]);
    store.stop();
  });
});

describe("AppStore: events", () => {
  it("refreshes the instance list when an instance changed", async () => {
    let list = [instance()];
    const t = baseTransport();
    t.on("instances.list", () => list);
    const { store } = await started(t);
    list = [withPhase("conflict")];
    t.emit({ type: "instance_changed", instance: list[0]!.id });
    await vi.waitFor(() => expect(store.getState().instances[0]?.phase).toBe("conflict"));
    store.stop();
  });

  it("bumps an instance's revision only when its status really changed", async () => {
    let list = [instance()];
    const t = baseTransport();
    t.on("instances.list", () => list);
    const { store } = await started(t);
    const id = list[0]!.id;
    const before = store.getState().revisions[id] ?? 0;
    await store.refreshInstances();
    expect(store.getState().revisions[id] ?? 0).toBe(before);
    list = [instance({ pending_snapshots: 1 })];
    await store.refreshInstances();
    expect(store.getState().revisions[id]).toBe(before + 1);
    store.stop();
  });

  it("re-reads the storage when the connection changed", async () => {
    let connected = true;
    const t = baseTransport();
    t.on("storage.status", () => ({ kind: "local", connected, detail: "x", account: null, space: null, needs_reconnect: false, oauth_client_configured: false, oauth_client_source: null, credential_store: null, problem: null, setup_hint: null }));
    const { store } = await started(t);
    connected = false;
    t.emit({ type: "storage_changed" });
    await vi.waitFor(() => expect(store.getState().storage?.connected).toBe(false));
    store.stop();
  });

  it("shows notices as toasts, at most five at a time", async () => {
    const { store, t } = await started();
    for (let i = 0; i < 8; i++) t.emit({ type: "notice", level: i === 7 ? "error" : "info", message: `n${i}` });
    const toasts = store.getState().toasts;
    expect(toasts).toHaveLength(5);
    expect(toasts.at(-1)).toMatchObject({ level: "error", message: "n7" });
    store.dismissToast(toasts[0]!.id);
    expect(store.getState().toasts).toHaveLength(4);
    store.stop();
  });

  it("catches up after the event stream comes back", async () => {
    const { store, t } = await started();
    const before = t.count("instances.list");
    t.setConnection("lost");
    expect(store.getState().connection).toBe("lost");
    t.setConnection("live");
    await vi.waitFor(() => expect(t.count("instances.list")).toBeGreaterThan(before));
    expect(store.getState().connection).toBe("live");
    store.stop();
  });

  it("polls for changes other processes (launcher hooks) made", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const t = baseTransport();
    const store = new AppStore(t, { pollMs: 5000 });
    await store.start();
    const before = t.count("instances.list");
    await vi.advanceTimersByTimeAsync(5200);
    expect(t.count("instances.list")).toBeGreaterThan(before);
    store.stop();
    const after = t.count("instances.list");
    await vi.advanceTimersByTimeAsync(20_000);
    expect(t.count("instances.list")).toBe(after);
  });
});

describe("AppStore: retrying", () => {
  it("runs a failed job again with the same arguments", async () => {
    const t = baseTransport();
    let calls = 0;
    t.on("instance.sync", () => {
      calls += 1;
      if (calls === 1) throw new ApiError({ code: "offline", message: "no network" });
      return { ok: true };
    });
    const { store } = await started(t);
    await expect(store.runJob("instance.sync", { instance: "i" })).rejects.toMatchObject({ code: "offline" });
    expect(store.canRetry("job-1")).toBe(true);
    await store.retryJob("job-1");
    expect(calls).toBe(2);
    expect(t.calls.filter((c) => c.job).map((c) => c.params)).toEqual([{ instance: "i" }, { instance: "i" }]);
    expect(store.getState().toasts.at(-1)).toMatchObject({ level: "success", message: "Done." });
    store.stop();
  });

  it("reports a second failure instead of hiding it, and cannot retry what it did not start", async () => {
    const t = baseTransport();
    t.on("instance.sync", () => {
      throw new ApiError({ code: "quota_exceeded", message: "Drive is full" });
    });
    const { store } = await started(t);
    await store.runJob("instance.sync", { instance: "i" }).catch(() => {});
    await store.retryJob("job-1");
    expect(store.getState().toasts.at(-1)).toMatchObject({ level: "error", message: "Drive is full" });
    expect(store.canRetry("job-from-elsewhere")).toBe(false);
    await store.retryJob("job-from-elsewhere"); // nothing happens
    store.stop();
  });
});
