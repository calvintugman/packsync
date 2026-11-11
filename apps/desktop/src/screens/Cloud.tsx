import { useState } from "react";
import type { CloudInstance } from "../api/types";
import { useLoad } from "../components/useLoad";
import { RestoreNewDialog } from "../dialogs/RestoreNewDialog";
import { formatBytes, formatRelative, packLine, plural, worldsLine } from "../lib/format";
import { href } from "../lib/router";
import { useAppState, useStore } from "../state/context";
import { Banner, Button, EmptyState, Pill } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";

export function CloudScreen() {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const instances = useAppState((s) => s.instances);
  const [bring, setBring] = useState<CloudInstance | null>(null);
  const connected = storage?.connected ?? false;
  const list = useLoad(() => (connected ? store.runJob("cloud.instances", undefined) : Promise.resolve([] as CloudInstance[])), `${connected}:${instances.length}`);
  const items = list.data ?? [];
  const usage = useLoad(() => (connected ? store.call("storage.usage") : Promise.resolve(null)), `${connected}:${items.length}`);
  const logical = items.reduce((n, c) => n + c.bytes, 0);

  return (
    <div className="page">
      <div className="page-head">
        <h1>In your cloud</h1>
        <Button icon="refresh" busy={list.loading} disabled={!connected} onClick={list.reload}>
          Refresh
        </Button>
      </div>
      <p className="muted">
        Everything PackSync keeps for you. Bring an instance to this computer to continue where another computer left off.
      </p>
      {connected && items.length > 0 ? (
        <div className="card row wrap" aria-label="Storage usage">
          <div className="stat">
            <div className="num">{formatBytes(logical)}</div>
            <div className="label">current size of {plural(items.length, "instance")} in the cloud</div>
          </div>
          {usage.data?.quota ? (
            <div className="stat">
              <div className="num">
                {formatBytes(usage.data.quota.used)}
                {usage.data.quota.total ? ` of ${formatBytes(usage.data.quota.total)}` : ""}
              </div>
              <div className="label">{storage?.kind === "google" ? "used in your Google Drive" : "used in the cloud"} (all files, history included)</div>
            </div>
          ) : null}
        </div>
      ) : null}
      {!connected ? (
        <Banner tone="info" title="No cloud is connected">
          Connect the cloud in Settings to see your instances here.
        </Banner>
      ) : null}
      {list.error ? <ErrorNote error={list.error} /> : null}
      {connected && items.length === 0 && !list.loading && !list.error ? (
        <div className="card">
          <EmptyState title="Nothing in the cloud yet">Start syncing an instance on any computer and it shows up here.</EmptyState>
        </div>
      ) : null}
      <div className="col gap-lg">
        {items.map((c) => (
          <article key={c.id} className="card col" aria-label={c.name}>
            <div className="row" style={{ alignItems: "flex-start" }}>
              <div className="grow">
                <strong style={{ fontSize: 16 }}>{c.name}</strong>
                <div className="meta">
                  {packLine(c.pack)} · {worldsLine(c.worlds)} · {plural(c.files, "file")} · {formatBytes(c.bytes)}
                </div>
                <div className="meta">
                  Last saved {formatRelative(c.last_updated)}
                  {c.last_device ? ` on ${c.last_device}` : ""}
                </div>
              </div>
              {c.heads.length > 1 ? <Pill tone="danger">Several versions</Pill> : null}
              {c.local_root ? <Pill tone="ok">On this computer</Pill> : null}
            </div>
            <div className="row">
              {c.local_root ? (
                <a className="btn" href={href({ name: "instance", id: c.id, tab: null })}>
                  Open it
                </a>
              ) : (
                <Button variant="primary" icon="download" onClick={() => setBring(c)}>
                  Bring to this computer…
                </Button>
              )}
              {c.local_root ? <code className="truncate">{c.local_root}</code> : null}
            </div>
          </article>
        ))}
      </div>
      {bring ? (
        <RestoreNewDialog
          cloud={bring}
          onClose={() => {
            setBring(null);
            list.reload();
          }}
        />
      ) : null}
    </div>
  );
}
