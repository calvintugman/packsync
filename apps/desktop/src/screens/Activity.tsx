import { useState } from "react";
import { describeError } from "../api/errors";
import type { JobInfo } from "../api/types";
import { JobProgress } from "../components/JobProgress";
import { useLoad } from "../components/useLoad";
import { formatDateTime, formatDuration, formatRelative, plural } from "../lib/format";
import { useAction, useAppState, useStore } from "../state/context";
import { jobTitle } from "../state/selectors";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { Button, EmptyState, Pill } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";

const DEFAULT_VISIBLE_JOBS = 6;

function defaultExportFilename(): string {
  const day = new Date().toISOString().slice(0, 10);
  return `packsync-logs-${day}.json`;
}

function downloadJsonBlob(filename: string, payload: unknown): void {
  const text = `${JSON.stringify(payload, null, 2)}\n`;
  const blob = new Blob([text], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

function JobRow({ job, name, instanceKnown }: { job: JobInfo; name: string; instanceKnown: boolean }) {
  const store = useStore();
  const err = job.error ? describeError(job.error) : null;
  const tone = job.state === "succeeded" ? "ok" : job.state === "failed" ? "danger" : job.state === "cancelled" ? "neutral" : "info";
  const label = job.state === "running" ? "Running" : job.state === "succeeded" ? "Done" : job.state === "failed" ? "Failed" : "Cancelled";
  const canRetry = (job.state === "failed" || job.state === "cancelled") && store.canRetry(job.id) && instanceKnown;

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
        {canRetry ? (
          <Button size="sm" onClick={() => void store.retryJob(job.id)}>
            Try again
          </Button>
        ) : null}
      </div>
      <div className="meta" title={formatDateTime(job.started_ms)}>
        {formatRelative(job.started_ms)}
        {job.finished_ms ? ` · ${formatDuration(job.finished_ms - job.started_ms)}` : ""}
      </div>
      {job.state === "running" ? <JobProgress job={job} /> : null}
      {err ? (
        <div className="col" style={{ gap: 2, marginTop: 4 }}>
          <span className="diff-before">
            {err.title}: {err.message}
          </span>
        </div>
      ) : null}
    </li>
  );
}

export function ActivityScreen() {
  const store = useStore();
  const jobs = useAppState((s) => s.jobs);
  const instances = useAppState((s) => s.instances);
  const info = useAppState((s) => s.info);
  const [showAllJobs, setShowAllJobs] = useState(false);
  const [showLogs, setShowLogs] = useState(false);
  const [onlyIssues, setOnlyIssues] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);

  const events = useLoad(() => store.call("events.recent", { limit: 100 }), Object.keys(jobs).length);
  const list = Object.values(jobs).sort((a, b) => b.started_ms - a.started_ms);
  const visibleJobs = showAllJobs ? list : list.slice(0, DEFAULT_VISIBLE_JOBS);
  const nameOf = (id: string) => instances.find((i) => i.id === id)?.name;

  const allLogs = events.data ?? [];
  const filteredLogs = onlyIssues ? allLogs.filter((e) => e.level === "warn" || e.level === "error") : allLogs;
  const issueCount = allLogs.filter((e) => e.level === "warn" || e.level === "error").length;

  const exportLogs = useAction(async () => {
    const filename = defaultExportFilename();
    if (store.transport.kind === "tauri" && store.transport.pickSaveFile) {
      const path = await store.transport.pickSaveFile("Export PackSync logs", filename, [
        { name: "JSON", extensions: ["json"] },
      ]);
      if (!path) return;
      const res = await store.call("events.export", { path, limit: 5000 });
      store.toast("success", `Exported ${plural(res.count, "log entry", "log entries")} to JSON.`);
      return;
    }
    const rows = await store.call("events.recent", { limit: 500 });
    downloadJsonBlob(filename, {
      exported_at_ms: Date.now(),
      version: info?.version ?? null,
      os: info?.os ?? null,
      arch: info?.arch ?? null,
      device: info?.device ?? null,
      count: rows.length,
      events: rows,
    });
    store.toast("success", `Exported ${plural(rows.length, "log entry", "log entries")} to JSON.`);
  });

  const clearLogs = async () => {
    const res = await store.call("events.clear");
    events.reload();
    store.toast("info", `Cleared ${plural(res.cleared, "log entry", "log entries")}.`);
  };

  return (
    <div className="page">
      <div className="page-head">
        <h1>Activity</h1>
        <Button icon="refresh" onClick={events.reload}>
          Refresh
        </Button>
      </div>

      <section className="col" aria-label="Recent operations">
        <div className="row">
          <h2 className="grow">Recent operations</h2>
          {list.length > DEFAULT_VISIBLE_JOBS ? (
            <Button size="sm" variant="ghost" onClick={() => setShowAllJobs((v) => !v)}>
              {showAllJobs ? "Show fewer" : `Show all (${list.length})`}
            </Button>
          ) : null}
        </div>
        {list.length === 0 ? (
          <div className="card">
            <EmptyState title="Nothing has run yet">Syncs, uploads and restores show up here while they run and afterwards.</EmptyState>
          </div>
        ) : (
          <div className="card flush">
            <ul className="list">
              {visibleJobs.map((j) => (
                <JobRow
                  key={j.id}
                  job={j}
                  name={jobTitle(j, nameOf)}
                  instanceKnown={!j.instance || instances.some((i) => i.id === j.instance)}
                />
              ))}
            </ul>
          </div>
        )}
      </section>

      <section className="col" aria-label="Detailed logs">
        <div className="card col">
          <div className="row wrap">
            <div className="col grow" style={{ gap: 2 }}>
              <div className="row" style={{ gap: 8 }}>
                <h2>Detailed logs</h2>
                <Pill tone={issueCount > 0 ? "warn" : "neutral"}>
                  {allLogs.length === 0 ? "Empty" : plural(allLogs.length, "entry", "entries")}
                </Pill>
              </div>
              <p className="muted" style={{ fontSize: 13 }}>
                Low-level sync and engine events for troubleshooting (secrets are redacted).
              </p>
            </div>
            <div className="row wrap">
              <Button
                size="sm"
                icon="download"
                busy={exportLogs.busy}
                disabled={allLogs.length === 0}
                onClick={() => void exportLogs.run()}
              >
                Export JSON
              </Button>
              <Button
                size="sm"
                variant="ghost"
                icon="trash"
                disabled={allLogs.length === 0}
                onClick={() => setConfirmClear(true)}
              >
                Clear logs
              </Button>
              <Button size="sm" aria-expanded={showLogs} onClick={() => setShowLogs((v) => !v)}>
                {showLogs ? "Hide logs" : "Show logs"}
              </Button>
            </div>
          </div>

          {events.error ? <ErrorNote error={events.error} /> : null}
          {exportLogs.error ? <ErrorNote error={exportLogs.error} /> : null}

          {showLogs ? (
            <div className="col" style={{ gap: 10 }}>
              <div className="row wrap">
                <Button size="sm" variant={!onlyIssues ? "primary" : "ghost"} onClick={() => setOnlyIssues(false)}>
                  All ({allLogs.length})
                </Button>
                <Button size="sm" variant={onlyIssues ? "primary" : "ghost"} onClick={() => setOnlyIssues(true)}>
                  Warnings &amp; errors ({issueCount})
                </Button>
              </div>
              <div className="card flush">
                <ul className="list">
                  {filteredLogs.map((e) => (
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
                  {filteredLogs.length === 0 ? (
                    <li className="muted">{onlyIssues ? "No warnings or errors." : "No log entries yet."}</li>
                  ) : null}
                </ul>
              </div>
            </div>
          ) : null}
        </div>
      </section>

      {confirmClear ? (
        <ConfirmDialog
          title="Clear detailed logs?"
          confirmLabel="Clear logs"
          danger
          onClose={() => setConfirmClear(false)}
          onConfirm={clearLogs}
        >
          <p>
            Removes all local detailed event logs from this computer. Your instances, snapshots, and cloud data are not
            touched.
          </p>
        </ConfirmDialog>
      ) : null}
    </div>
  );
}
