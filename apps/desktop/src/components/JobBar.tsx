import { useAppState, useStore } from "../state/context";
import { Button } from "../ui/controls";
import { JobProgress } from "./JobProgress";

/** Progress of one job, with a Cancel button while it is running. */
export function JobBar({ jobId, cancellable = true }: { jobId: string | null; cancellable?: boolean }) {
  const store = useStore();
  const job = useAppState((s) => (jobId ? s.jobs[jobId] : undefined));
  if (!jobId || !job || job.state !== "running") {
    return (
      <div className="row" role="status">
        <span className="spinner" aria-hidden="true" />
        <span>Working…</span>
      </div>
    );
  }
  return (
    <div className="row" style={{ alignItems: "flex-end" }}>
      <div className="grow">
        <JobProgress job={job} />
      </div>
      {cancellable ? (
        <Button size="sm" onClick={() => void store.cancelJob(job.id).catch(() => {})}>
          Cancel
        </Button>
      ) : null}
    </div>
  );
}
