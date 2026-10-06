import { describeError } from "../api/errors";
import type { JobInfo } from "../api/types";
import { JobProgress } from "../components/JobProgress";
import { useLoad } from "../components/useLoad";
import { formatDateTime, formatDuration, formatRelative } from "../lib/format";
import { useAppState, useStore } from "../state/context";
import { jobTitle } from "../state/selectors";
import { Button, EmptyState, Pill } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";

function JobRow({ job, name }: { job: JobInfo; name: string }) {
  const store = useStore();
  const err = job.error ? describeError(job.error) : null;
  const tone = job.state === "succeeded" ? "ok" : job.state === "failed" ? "danger" : job.state === "cancelled" ? "neutral" : "info";
  const label = job.state === "running" ? "Running" : job.state === "succeeded" ? "Done" : job.state === "failed" ? "Failed" : "Cancelled";
  return (
    <li data-job={job.method} data-state={job.state}>
      <div className="row">
        <strong className="grow truncate">{name}</strong>
        <Pill tone={tone}>{label}</Pill>
        {job.state === "running" && job.method !== "launch.start" ? (
          <Button size="sm" onClick={() => void store.cancelJob(job.id).catch(() => {})}>
            Cancel
          </Button>
        ) : null}
        {(job.state === "failed" || job.state === "cancelled") && store.canRetry(job.id) ? (
          <Button size="sm" onClick={() => void store.retryJob(job.id)}>
            Try again
          </Button>
        ) : null}
      </div>
      <div className="meta" title={formatDateTime(job.started_ms)}>
        Started {formatRelative(job.started_ms)}
        {job.finished_ms ? ` · took ${formatDuration(job.finished_ms - job.started_ms)}` : ""}
      </div>
      {job.state === "running" ? <JobProgress job={job} /> : null}
      {err ? (
        <div className="col" style={{ gap: 2, marginTop: 4 }}>
          <span className="diff-before">{err.title}: {err.message}</span>
          {err.advice ? <span className="muted">{err.advice}</span> : null}
        </div>
      ) : null}
    </li>
  );
}

export function ActivityScreen() {
  const store = useStore();
  const jobs = useAppState((s) => s.jobs);
  const instances = useAppState((s) => s.instances);
  const events = useLoad(() => store.call("events.recent", { limit: 40 }), Object.keys(jobs).length);
  const list = Object.values(jobs).sort((a, b) => b.started_ms - a.started_ms);
  const nameOf = (id: string) => instances.find((i) => i.id === id)?.name;
  return (
    <div className="page">
      <div className="page-head">
        <h1>Activity</h1>
        <Button icon="refresh" onClick={events.reload}>
          Refresh
        </Button>
      </div>
      <section className="col" aria-label="Operations">
        <h2>Operations</h2>
        {list.length === 0 ? (
          <div className="card">
            <EmptyState title="Nothing has run yet">Syncs, uploads and restores show up here while they run and afterwards.</EmptyState>
          </div>
        ) : (
          <div className="card flush">
            <ul className="list">
              {list.map((j) => (
                <JobRow key={j.id} job={j} name={jobTitle(j, nameOf)} />
              ))}
            </ul>
          </div>
        )}
      </section>
      <section className="col" aria-label="Recent events">
        <h2>Recent events</h2>
        {events.error ? <ErrorNote error={events.error} /> : null}
        <div className="card flush">
          <ul className="list">
            {(events.data ?? []).map((e) => (
              <li key={`${e.ts_ms}-${e.code}-${e.message}`} className="row" style={{ alignItems: "flex-start" }}>
                <Pill tone={e.level === "error" ? "danger" : e.level === "warn" ? "warn" : "neutral"}>{e.level}</Pill>
                <div className="grow">
                  <div>{e.message}</div>
                  <div className="meta">
                    {e.code} · {formatRelative(e.ts_ms)}
                    {e.instance ? ` · ${nameOf(e.instance) ?? e.instance.slice(0, 8)}` : ""}
                  </div>
                </div>
              </li>
            ))}
            {(events.data ?? []).length === 0 ? <li className="muted">No events yet.</li> : null}
          </ul>
        </div>
      </section>
    </div>
  );
}
