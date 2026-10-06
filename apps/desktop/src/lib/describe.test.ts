import { describe, expect, it } from "vitest";
import type { LaunchOutcome, ReconcileReport, SyncResult } from "../api/types";
import { describeLaunch, describeReconcile, describeSync, describeUpload } from "./describe";

const head = "aa".repeat(32);

describe("describeReconcile", () => {
  it("is honest about every outcome", () => {
    expect(describeReconcile({ outcome: "up_to_date", head })).toMatchObject({ tone: "ok" });
    expect(describeReconcile({ outcome: "cloud_empty" })).toMatchObject({ tone: "info" });
    expect(describeReconcile({ outcome: "cloud_ahead", head })).toMatchObject({ tone: "info" });
    expect(describeReconcile({ outcome: "offline", base: null, reason: "no network" })).toMatchObject({ tone: "warn" });
    const c: ReconcileReport = { outcome: "conflict", kind: "diverged", local: null, local_dirty: false, cloud: [] };
    expect(describeReconcile(c)).toMatchObject({ tone: "danger" });
  });

  it("does not claim an upload that did not happen", () => {
    const failed = describeReconcile({ outcome: "local_ahead", snapshots: [head], uploaded: false, upload_error: "Google Drive is full" });
    expect(failed.tone).toBe("warn");
    expect(failed.text).toContain("Google Drive is full");
    expect(describeReconcile({ outcome: "local_ahead", snapshots: [head], uploaded: true, upload_error: null }).tone).toBe("ok");
  });

  it("counts files of a fast-forward", () => {
    const o = describeReconcile({ outcome: "fast_forwarded", from: head, to: head, files_written: 3, files_deleted: 1 });
    expect(o.text).toContain("3 files updated");
    expect(o.text).toContain("1 file removed");
  });
});

describe("describeSync", () => {
  it("mentions saved changes when the cloud is up to date", () => {
    const r: SyncResult = {
      snapshot: { result: { result: "created", id: head }, stats: {} as never, issues: [] },
      reconcile: { outcome: "up_to_date", head },
    };
    expect(describeSync(r).text).toContain("Saved your changes");
  });
});

describe("describeUpload and describeLaunch", () => {
  it("reports deferral and size", () => {
    expect(describeUpload({ snapshots: [], bytes_uploaded: 0, deferred_until_ms: Date.now() + 1000, remaining: 1 }).tone).toBe("warn");
    expect(describeUpload({ snapshots: [], bytes_uploaded: 0, deferred_until_ms: null, remaining: 0 }).text).toBe("Nothing to upload.");
    expect(describeUpload({ snapshots: [{} as never], bytes_uploaded: 2_500_000, deferred_until_ms: null, remaining: 0 }).text).toBe("Uploaded 2.5 MB.");
  });

  const base: LaunchOutcome = {
    session: "s", pre_sync: null, offline: false, playing_elsewhere: [], game_started: true,
    launcher_exit_code: 0, clean_signal: false, end: null, warnings: [],
  };
  it("flags a session that did not end normally", () => {
    const crashed = describeLaunch({ ...base, end: { outcome: "crashed", snapshot: null, kind: "crash", crash_artifacts: [], uploaded: false, upload_error: null } });
    expect(crashed.tone).toBe("danger");
    expect(crashed.text).toContain("did not end normally");
  });
  it("distinguishes saved-and-uploaded from saved-only", () => {
    const end = { outcome: "clean" as const, snapshot: null, kind: "normal" as const, crash_artifacts: [], uploaded: true, upload_error: null };
    expect(describeLaunch({ ...base, end }).text).toBe("Session saved and uploaded.");
    expect(describeLaunch({ ...base, end: { ...end, uploaded: false, upload_error: "offline" } }).tone).toBe("warn");
    expect(describeLaunch({ ...base, game_started: false }).tone).toBe("warn");
  });
});
