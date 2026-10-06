import { describe, expect, it } from "vitest";
import type { SyncPhase } from "../api/types";
import { withPhase } from "../test/fixtures";
import { canPlay, needsAttention, PHASES, STEPS } from "./phases";

const ALL: SyncPhase[] = [
  "not_enabled",
  "local_snapshot_complete",
  "upload_pending",
  "cloud_published",
  "ready_on_other_devices",
  "cloud_ahead",
  "conflict",
  "crash_pending",
  "restoring",
  "unavailable",
];

describe("sync phases", () => {
  it("describes every phase the engine can report", () => {
    for (const p of ALL) {
      expect(PHASES[p].label.length).toBeGreaterThan(3);
      expect(PHASES[p].summary.length).toBeGreaterThan(10);
    }
  });

  it("maps the four-stage journey in order", () => {
    expect(PHASES.local_snapshot_complete.step).toBe(0);
    expect(PHASES.upload_pending.step).toBe(1);
    expect(PHASES.cloud_published.step).toBe(2);
    expect(PHASES.ready_on_other_devices.step).toBe(3);
    expect(STEPS).toHaveLength(4);
  });

  it("never calls an unfinished upload synced", () => {
    for (const p of ["local_snapshot_complete", "upload_pending", "cloud_published"] as const) {
      expect(PHASES[p].tone).not.toBe("ok");
    }
    expect(PHASES.ready_on_other_devices.tone).toBe("ok");
  });

  it("asks for attention on conflicts, crashes and missing folders only", () => {
    expect(needsAttention(withPhase("conflict"))).toBe(true);
    expect(needsAttention(withPhase("crash_pending"))).toBe(true);
    expect(needsAttention(withPhase("unavailable"))).toBe(true);
    expect(needsAttention(withPhase("cloud_ahead"))).toBe(false);
    expect(needsAttention(withPhase("ready_on_other_devices"))).toBe(false);
  });

  it("only offers Play where the engine can start a session", () => {
    expect(canPlay(withPhase("ready_on_other_devices"))).toBe(true);
    expect(canPlay(withPhase("unavailable"))).toBe(false);
    expect(canPlay(withPhase("restoring", { state: "restoring" }))).toBe(false);
    expect(canPlay(withPhase("not_enabled", { sync_enabled: false }))).toBe(false);
  });
});
