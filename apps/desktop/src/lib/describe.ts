import type { LaunchOutcome, ReconcileReport, SyncResult, UploadSummary } from "../api/types";
import { formatBytes, plural } from "./format";

export type Outcome = { tone: "ok" | "info" | "warn" | "danger"; text: string };

/** What a reconcile (cloud check) result means for the user, in one sentence. */
export function describeReconcile(r: ReconcileReport): Outcome {
  switch (r.outcome) {
    case "up_to_date":
      return { tone: "ok", text: "Everything is up to date." };
    case "cloud_empty":
      return { tone: "info", text: "Nothing is in the cloud yet." };
    case "local_ahead":
      if (r.uploaded) return { tone: "ok", text: "Your latest progress is uploaded." };
      return {
        tone: "warn",
        text: r.upload_error
          ? `Saved on this computer, but the upload did not finish: ${r.upload_error}`
          : "Saved on this computer. The upload has not happened yet.",
      };
    case "cloud_ahead":
      return { tone: "info", text: "The cloud has newer progress from another computer." };
    case "fast_forwarded":
      return {
        tone: "ok",
        text: `Applied newer progress from the cloud (${plural(r.files_written, "file")} updated${
          r.files_deleted ? `, ${plural(r.files_deleted, "file")} removed` : ""
        }).`,
      };
    case "conflict":
      return { tone: "danger", text: "This computer and the cloud both changed. Choose which version to keep." };
    case "offline":
      return { tone: "warn", text: `The cloud can't be reached (${r.reason}). Your local progress is safe.` };
  }
}

export function describeSync(r: SyncResult): Outcome {
  const base = describeReconcile(r.reconcile);
  if (r.reconcile.outcome === "up_to_date" && r.snapshot.result.result === "created") {
    return { tone: "ok", text: "Saved your changes and everything is up to date." };
  }
  return base;
}

export function describeUpload(u: UploadSummary): Outcome {
  if (u.deferred_until_ms) return { tone: "warn", text: "The upload is waiting before it retries." };
  if (u.snapshots.length === 0) return { tone: "ok", text: "Nothing to upload." };
  return { tone: "ok", text: `Uploaded ${formatBytes(u.bytes_uploaded)}.` };
}

export function describeLaunch(o: LaunchOutcome): Outcome {
  if (!o.game_started) return { tone: "warn", text: "The game did not start." };
  const end = o.end;
  if (!end) return { tone: "info", text: "The game is running." };
  if (end.outcome !== "clean") {
    return {
      tone: "danger",
      text: "The session did not end normally. PackSync saved what it found as a crash save and is waiting for your decision.",
    };
  }
  if (end.uploaded) return { tone: "ok", text: "Session saved and uploaded." };
  if (end.upload_error) return { tone: "warn", text: `Session saved here. The upload did not finish: ${end.upload_error}` };
  if (o.offline) {
    return {
      tone: "warn",
      text: "Played offline. Your progress is saved on this computer and uploads once the cloud is reachable.",
    };
  }
  return { tone: "ok", text: "Session saved." };
}
