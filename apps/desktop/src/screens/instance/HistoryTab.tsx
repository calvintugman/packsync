import { useState } from "react";
import type { HistoryEntry, InstanceId, SnapshotState } from "../../api/types";
import { useLoad } from "../../components/useLoad";
import { RestoreDialog } from "../../dialogs/RestoreDialog";
import { formatBytes, formatDateTime, formatRelative, plural } from "../../lib/format";
import { useStore } from "../../state/context";
import { Button, EmptyState, Pill, Tag } from "../../ui/controls";
import { ErrorNote } from "../../ui/ErrorNote";

const STATE_LABEL: Record<SnapshotState, { label: string; tone: "ok" | "info" | "warn" | "neutral" }> = {
  local_complete: { label: "Saved here only", tone: "warn" },
  uploading: { label: "Uploading", tone: "warn" },
  published: { label: "In the cloud", tone: "info" },
  visible: { label: "In the cloud", tone: "ok" },
  remote: { label: "In the cloud", tone: "ok" },
};

function restoreBlock(e: HistoryEntry): string | null {
  if (e.is_base) return "This is where the instance is now.";
  if (e.expired) return "Its content was released by a cloud cleanup.";
  if (e.state === "local_complete" || e.state === "uploading") return "Upload it first: it exists only on this computer.";
  return null;
}

/** Every snapshot of the instance, newest first, with the one the computer is on marked. */
export function HistoryTab({ instance, revision }: { instance: InstanceId; revision: number }) {
  const store = useStore();
  const [online, setOnline] = useState(false);
  const [restore, setRestore] = useState<HistoryEntry | null>(null);
  const h = useLoad(() => store.call("instance.history", { instance, online }), `${instance}:${revision}:${online}`);
  const entries = h.data ?? [];

  return (
    <div className="col">
      <div className="row">
        <p className="muted grow">
          Every save is kept. Restoring one first saves what you have now, so you can always come back.
        </p>
        <Button size="sm" busy={h.loading} onClick={() => (online ? h.reload() : setOnline(true))}>
          {online ? "Refresh" : "Include the cloud"}
        </Button>
      </div>
      {h.error ? <ErrorNote error={h.error} instanceId={instance} /> : null}
      {entries.length === 0 && !h.loading ? (
        <div className="card">
          <EmptyState title="No snapshots yet">The first one is created when syncing starts.</EmptyState>
        </div>
      ) : (
        <div className="card flush">
          <ul className="list" aria-label="Snapshots">
            {entries.map((e) => {
              const s = STATE_LABEL[e.state];
              const block = restoreBlock(e);
              return (
                <li key={e.id} className="row" data-snapshot={e.id}>
                  <div className="grow">
                    <div className="row wrap" style={{ gap: 8 }}>
                      <strong title={formatDateTime(e.created)}>{formatRelative(e.created)}</strong>
                      {e.device ? <span className="muted">on {e.device}</span> : null}
                      {e.is_base ? <Tag title="This computer's instance is at this snapshot">This computer</Tag> : null}
                      {e.is_cloud_head ? <Tag title="The newest version in the cloud">Latest in the cloud</Tag> : null}
                      {e.kind === "crash" ? <Pill tone="danger">Crash save</Pill> : null}
                      {e.expired ? <Pill>Released</Pill> : null}
                    </div>
                    <div className="meta">
                      {plural(e.files, "file")} · {formatBytes(e.bytes)}
                      {e.note ? <> · {e.note}</> : null}
                    </div>
                  </div>
                  <Pill tone={s.tone}>{s.label}</Pill>
                  <Button size="sm" disabled={block !== null} title={block ?? undefined} onClick={() => setRestore(e)}>
                    Restore…
                  </Button>
                </li>
              );
            })}
          </ul>
        </div>
      )}
      {restore ? (
        <RestoreDialog
          instance={instance}
          entry={restore}
          onClose={() => {
            setRestore(null);
            h.reload();
          }}
        />
      ) : null}
    </div>
  );
}
