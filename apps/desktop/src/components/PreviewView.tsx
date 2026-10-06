import { baseName, formatBytes, packLine, plural, worldName } from "../lib/format";
import type { PreviewReport } from "../api/types";
import { Banner, Kv } from "../ui/controls";

export const CATEGORY_LABELS: Record<string, string> = {
  world: "Worlds",
  mods: "Mods",
  config: "Pack configuration",
  pack: "Pack definition",
  progress: "Progress outside worlds (waypoints, schematics…)",
  settings: "Settings",
  machine: "Settings specific to one computer",
  log: "Logs",
  crash: "Crash reports",
  cache: "Caches",
  backup: "Other backups",
  screenshot: "Screenshots",
  shader: "Shader packs",
  native: "Native libraries and executables",
  "launcher-managed": "Managed by the launcher",
  temp: "Temporary files",
  lock: "Lock files",
};

export function categoryLabel(c: string): string {
  return CATEGORY_LABELS[c] ?? c;
}

/** What would be synced and what would not, with the reasons. */
export function PreviewView({ report }: { report: PreviewReport }) {
  const excluded = Object.entries(report.excluded.groups)
    .map(([id, g]) => ({ id, ...g }))
    .filter((g) => g.files > 0)
    .sort((a, b) => b.bytes - a.bytes);
  const unmatched = Object.entries(report.excluded.unmatched_by_dir).sort((a, b) => b[1][1] - a[1][1]);
  return (
    <div className="col" style={{ gap: 16 }}>
      <div>
        <h3>{report.name}</h3>
        <p className="meta">{packLine(report.pack)}</p>
      </div>
      <div className="stat-row">
        <div className="stat">
          <div className="num">{report.included_files.toLocaleString("en")}</div>
          <div className="label">files will be synced</div>
        </div>
        <div className="stat">
          <div className="num">{formatBytes(report.included_bytes)}</div>
          <div className="label">in total</div>
        </div>
        <div className="stat">
          <div className="num">{plural(report.worlds.length, "world")}</div>
          <div className="label">{report.worlds.map(worldName).slice(0, 3).join(", ") || "none found yet"}</div>
        </div>
        {report.estimated_upload_bytes !== null ? (
          <div className="stat">
            <div className="num">{formatBytes(report.estimated_upload_bytes)}</div>
            <div className="label">to upload first</div>
          </div>
        ) : null}
      </div>

      {report.issues.length > 0 ? (
        <Banner tone="warn" title={`${plural(report.issues.length, "file")} need attention`}>
          <ul style={{ margin: "4px 0 0", paddingLeft: 18 }}>
            {report.issues.slice(0, 6).map((i) => (
              <li key={i.path + i.kind}>
                <code>{i.path}</code> — {i.detail}
              </li>
            ))}
            {report.issues.length > 6 ? <li>…and {report.issues.length - 6} more</li> : null}
          </ul>
        </Banner>
      ) : null}

      <div>
        <h3 style={{ marginBottom: 6 }}>Synced</h3>
        <table className="table">
          <thead>
            <tr>
              <th>What</th>
              <th className="num">Files</th>
              <th className="num">Size</th>
            </tr>
          </thead>
          <tbody>
            {report.included_by_category.map((g) => (
              <tr key={g.category}>
                <td>{categoryLabel(g.category)}</td>
                <td className="num">{g.files.toLocaleString("en")}</td>
                <td className="num">{formatBytes(g.bytes)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div>
        <h3 style={{ marginBottom: 6 }}>Not synced</h3>
        {excluded.length === 0 && unmatched.length === 0 && report.excluded.pruned_dirs.length === 0 ? (
          <p className="muted">Nothing is excluded.</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>What</th>
                <th>Why</th>
                <th className="num">Size</th>
              </tr>
            </thead>
            <tbody>
              {excluded.slice(0, 10).map((g) => (
                <tr key={g.id}>
                  <td>{categoryLabel(g.category)}</td>
                  <td className="muted">{g.reason}</td>
                  <td className="num">{formatBytes(g.bytes)}</td>
                </tr>
              ))}
              {report.excluded.pruned_dirs.slice(0, 12).map(([path, id, reason]) => (
                <tr key={`${id}:${path}`}>
                  <td>
                    <code>{baseName(path)}/</code>
                  </td>
                  <td className="muted">{reason}</td>
                  <td className="num faint">skipped</td>
                </tr>
              ))}
              {excluded.length > 10 ? (
                <tr>
                  <td colSpan={3} className="muted">
                    …and {excluded.length - 10} more groups
                  </td>
                </tr>
              ) : null}
            </tbody>
          </table>
        )}
        {unmatched.length > 0 ? (
          <Banner tone="info" title="Folders no rule recognises">
            These are not synced. Add a rule if you want them: {" "}
            {unmatched
              .slice(0, 6)
              .map(([dir, [files, bytes]]) => `${baseName(dir) || dir} (${plural(files, "file")}, ${formatBytes(bytes)})`)
              .join("; ")}
            .
          </Banner>
        ) : null}
      </div>

      <details>
        <summary>
          <strong>Sample of synced files</strong>
        </summary>
        <Kv rows={report.samples.slice(0, 12).map(([cat, path, size]) => [categoryLabel(cat), <span key={path}><code>{path}</code> · {formatBytes(size)}</span>])} />
      </details>
    </div>
  );
}
