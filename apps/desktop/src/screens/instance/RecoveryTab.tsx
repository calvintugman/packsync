import type { InstanceId } from "../../api/types";
import { useLoad } from "../../components/useLoad";
import { formatBytes, formatDateTime, plural } from "../../lib/format";
import { useStore } from "../../state/context";
import { EmptyState } from "../../ui/controls";
import { ErrorNote } from "../../ui/ErrorNote";

/** Copies of files a restore replaced. They are kept for a while so a restore is never a one-way door. */
export function RecoveryTab({ instance, revision }: { instance: InstanceId; revision: number }) {
  const store = useStore();
  const points = useLoad(() => store.call("restore.points", { instance }), `${instance}:${revision}`);
  const list = points.data ?? [];
  return (
    <div className="col">
      <p className="muted">
        Before a restore replaces or removes files, PackSync keeps the originals here for a while (the retention is set
        in Settings). To get a file back, copy it from the folder shown.
      </p>
      {points.error ? <ErrorNote error={points.error} /> : null}
      {list.length === 0 && !points.loading ? (
        <div className="card">
          <EmptyState title="No recovery points">They appear when a restore replaces files.</EmptyState>
        </div>
      ) : (
        <div className="card flush">
          <ul className="list" aria-label="Recovery points">
            {list.map((p) => (
              <li key={p.tx}>
                <div className="row wrap">
                  <strong>{formatDateTime(p.created)}</strong>
                  <span className="muted">
                    {plural(p.files, "file")} · {formatBytes(p.bytes)} · {p.reason}
                  </span>
                </div>
                <code>{p.path}</code>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
