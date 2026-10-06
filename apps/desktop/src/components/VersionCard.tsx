import type { SnapshotSummary } from "../api/types";
import { formatBytes, formatDateTime, formatRelative, plural, worldName } from "../lib/format";

/** One version of an instance (this computer's, or one the cloud has) for side-by-side comparison. */
export function VersionCard({ title, v, note }: { title: string; v: SnapshotSummary | null; note?: string }) {
  return (
    <div className="card col" style={{ gap: 8 }} aria-label={title}>
      <h3>{title}</h3>
      {v ? (
        <>
          <div className="meta">
            Saved {formatRelative(v.created)}
            {v.device ? <> on <strong>{v.device}</strong></> : null}
            <div className="faint">{formatDateTime(v.created)}</div>
          </div>
          <div>
            {plural(v.files, "file")} · {formatBytes(v.bytes)}
          </div>
          {v.worlds.length > 0 ? (
            <ul style={{ margin: 0, paddingLeft: 18 }}>
              {v.worlds.map((w) => (
                <li key={w.folder}>
                  {worldName(w)} <span className="muted">— last played {w.last_played_ms ? formatRelative(w.last_played_ms) : "never"}</span>
                </li>
              ))}
            </ul>
          ) : (
            <span className="muted">No worlds</span>
          )}
        </>
      ) : (
        <span className="muted">No saved version yet.</span>
      )}
      {note ? <div className="banner info">{note}</div> : null}
    </div>
  );
}
