import { useEffect, useRef, useState } from "react";
import type { HistoryEntry, InstanceId, RestorePreview, RestoreReport } from "../api/types";
import { JobBar } from "../components/JobBar";
import { formatBytes, formatDateTime, formatRelative, plural } from "../lib/format";
import { useAction, useStore } from "../state/context";
import { Banner, Button, Checkbox } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Go back to (or forward to) another snapshot: show exactly what changes first, keep a recovery point. */
export function RestoreDialog({ instance, entry, onClose }: { instance: InstanceId; entry: HistoryEntry; onClose: () => void }) {
  const store = useStore();
  const [preview, setPreview] = useState<RestorePreview | null>(null);
  const [skip, setSkip] = useState(false);
  const [done, setDone] = useState<RestoreReport | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);

  const load = useAction(async () => {
    setPreview(await store.runJob("restore.preview", { instance, snapshot: entry.id }, { onStart: setJobId }));
  });
  const apply = useAction(async () => {
    const r = await store.runJob("restore.apply", { instance, snapshot: entry.id, skip_incompatible: skip }, { onStart: setJobId });
    setDone(r);
    store.toast("success", "Restored.");
    await store.refreshInstances();
  });

  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void load.run();
  }, [load]);

  const blocked = (preview?.problems.length ?? 0) > 0 || ((preview?.skipped.length ?? 0) > 0 && !skip);
  return (
    <Dialog
      title="Restore this version"
      onClose={onClose}
      wide
      locked={apply.busy}
      footer={
        done ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button onClick={onClose} disabled={apply.busy}>
              Cancel
            </Button>
            <Button variant="primary" busy={apply.busy} disabled={!preview || blocked} onClick={() => void apply.run()}>
              Restore
            </Button>
          </>
        )
      }
    >
      <div className="meta">
        Saved {formatRelative(entry.created)}
        {entry.device ? <> on <strong>{entry.device}</strong></> : null} · {formatDateTime(entry.created)}
      </div>
      {load.busy || apply.busy ? <JobBar jobId={jobId} /> : null}
      {load.error ? <ErrorNote error={load.error} instanceId={instance} /> : null}
      {apply.error ? <ErrorNote error={apply.error} instanceId={instance} /> : null}
      {done ? (
        <Banner tone="ok" title="Restored">
          {plural(done.written, "file")} written, {plural(done.deleted, "file")} removed.
          {done.bytes_reused_locally > 0 ? ` ${formatBytes(done.bytes_reused_locally)} was already on disk and not downloaded.` : ""}
          {done.recovery_point ? ` What was replaced is kept as a recovery point (${done.recovery_point}) for a while.` : ""}
        </Banner>
      ) : null}
      {preview && !done ? (
        <>
          <div className="stat-row">
            <div className="stat">
              <div className="num">{preview.files_to_write.toLocaleString("en")}</div>
              <div className="label">files will be written ({formatBytes(preview.bytes_to_write)})</div>
            </div>
            <div className="stat">
              <div className="num">{preview.files_to_delete.toLocaleString("en")}</div>
              <div className="label">files will be removed</div>
            </div>
            <div className="stat">
              <div className="num">{preview.files_unchanged.toLocaleString("en")}</div>
              <div className="label">stay as they are</div>
            </div>
          </div>
          {preview.local_changes ? (
            <Banner tone="info" title="You have changes that are not saved yet">
              They are saved as a snapshot first, so you can come back to them.
            </Banner>
          ) : null}
          {preview.local_only_preserved > 0 ? (
            <p className="muted">
              {plural(preview.local_only_preserved, "file")} that exist only on this computer (like logs and caches) are left alone.
            </p>
          ) : null}
          {preview.problems.length > 0 ? (
            <Banner tone="danger" title="This can't be restored right now">
              <ul style={{ margin: 0, paddingLeft: 18 }}>
                {preview.problems.map((p) => (
                  <li key={p}>{p}</li>
                ))}
              </ul>
            </Banner>
          ) : null}
          {preview.skipped.length > 0 ? (
            <Banner tone="warn" title={`${plural(preview.skipped.length, "file")} can't be written on this computer`}>
              <ul style={{ margin: "4px 0 8px", paddingLeft: 18 }}>
                {preview.skipped.slice(0, 5).map((s) => (
                  <li key={s.path}>
                    <code>{s.path}</code> — {s.problem}
                  </li>
                ))}
              </ul>
              <Checkbox checked={skip} onChange={setSkip}>
                Skip these files and restore everything else
              </Checkbox>
            </Banner>
          ) : null}
          {preview.sample_writes.length > 0 || preview.sample_deletes.length > 0 ? (
            <details>
              <summary>
                <strong>Examples of what changes</strong>
              </summary>
              <ul>
                {preview.sample_writes.slice(0, 8).map((p) => (
                  <li key={`w${p}`}>
                    <span className="diff-after">write</span> <code>{p}</code>
                  </li>
                ))}
                {preview.sample_deletes.slice(0, 8).map((p) => (
                  <li key={`d${p}`}>
                    <span className="diff-before">remove</span> <code>{p}</code>
                  </li>
                ))}
              </ul>
            </details>
          ) : null}
        </>
      ) : null}
    </Dialog>
  );
}
