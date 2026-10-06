import type { JobInfo } from "../api/types";
import { formatBytes } from "../lib/format";
import { useAppState } from "../state/context";
import { ProgressBar } from "../ui/controls";

const PHASE_TEXT: Record<string, string> = {
  discovery: "Looking around",
  scan: "Checking files",
  snapshot: "Saving a snapshot",
  upload: "Uploading",
  publish: "Publishing",
  reconcile: "Comparing with the cloud",
  stage_download: "Downloading",
  validate: "Checking what was downloaded",
  apply: "Applying changes",
  recover: "Recovering",
  verify: "Verifying",
  cleanup: "Cleaning up",
  conflict: "Resolving a conflict",
  launch: "Playing",
  idle: "Working",
};

/** The latest progress of a running job: what it is doing, with a bar when the total is known. */
export function JobProgress({ job }: { job: JobInfo }) {
  const p = useAppState((s) => s.progress[job.id] ?? job.progress ?? null);
  const what = p ? p.message || PHASE_TEXT[p.phase] || "Working" : "Starting";
  let value: number | null = null;
  let detail = "";
  if (p && p.bytes_total > 0) {
    value = p.bytes_done / p.bytes_total;
    detail = `${formatBytes(p.bytes_done)} of ${formatBytes(p.bytes_total)}`;
  } else if (p && p.items_total > 0) {
    value = p.items_done / p.items_total;
    detail = `${p.items_done.toLocaleString("en")} of ${p.items_total.toLocaleString("en")}`;
  }
  return (
    <div className="col" style={{ gap: 6 }}>
      <div className="row">
        <span className="truncate grow">{what}</span>
        {detail ? <span className="muted">{detail}</span> : null}
      </div>
      <ProgressBar value={value} label={what} />
    </div>
  );
}
