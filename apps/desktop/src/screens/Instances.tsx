import { useEffect, useState } from "react";
import type { StorageUsage } from "../api/types";
import { ConnectionCard } from "../components/ConnectionCard";
import { InstanceCard } from "../components/InstanceCard";
import { EnableDialog } from "../dialogs/EnableDialog";
import { ManualInstanceDialog } from "../dialogs/ManualInstanceDialog";
import { formatBytes, packLine, plural, worldsLine } from "../lib/format";
import { needsAttention } from "../lib/phases";
import { href, navigate } from "../lib/router";
import { useAppState, useStore } from "../state/context";
import { Banner, Button, EmptyState } from "../ui/controls";
import { Icon } from "../ui/Icon";

export function InstancesScreen() {
  const store = useStore();
  const instances = useAppState((s) => s.instances);
  const storage = useAppState((s) => s.storage);
  const discovered = useAppState((s) => s.discovered);
  const [enableRoot, setEnableRoot] = useState<string | null>(null);
  const [manual, setManual] = useState(false);
  const [usage, setUsage] = useState<StorageUsage | null>(null);

  useEffect(() => {
    void store.refreshDiscovered().catch(() => {});
  }, [store]);

  useEffect(() => {
    let alive = true;
    if (storage?.connected) {
      store
        .call("storage.usage")
        .then((u) => {
          if (alive) setUsage(u);
        })
        .catch(() => {});
    }
    return () => {
      alive = false;
    };
  }, [store, storage?.connected, instances.length]);

  const activeUsage = storage?.connected ? usage : null;

  const synced = instances.filter((i) => i.sync_enabled);
  const sorted = [...synced].sort((a, b) => Number(needsAttention(b)) - Number(needsAttention(a)) || a.name.localeCompare(b.name));
  const unsynced = (discovered ?? []).filter((d) => !d.sync_enabled);

  const attentionCount = synced.filter(needsAttention).length;
  const totalBytes = synced.reduce((n, i) => n + i.total_bytes, 0);
  const totalFiles = synced.reduce((n, i) => n + i.total_files, 0);
  const totalWorlds = synced.reduce((n, i) => n + i.worlds.length, 0);
  const pendingSnapshots = synced.reduce((n, i) => n + i.pending_snapshots, 0);
  const pendingBytes = synced.reduce((n, i) => n + i.pending_pack_bytes, 0);

  return (
    <div className="dashboard-page">
      <div className="dashboard-layout">
        <div className="dashboard-main">
          <div className="section-head">
            <h1>Instances</h1>
            <Button icon="plus" onClick={() => setManual(true)}>
              Add a folder…
            </Button>
            <a className="section-link" href={href({ name: "cloud" })}>
              See all →
            </a>
          </div>

          {storage?.kind === "none" ? (
            <Banner
              tone="info"
              title="Connect your cloud to start syncing"
              actions={
                <Button size="sm" variant="primary" onClick={() => navigate({ name: "setup" })}>
                  Set up
                </Button>
              }
            >
              PackSync keeps your worlds in the cloud, encrypted. Until you connect, snapshots stay on this computer.
            </Banner>
          ) : null}
          {storage?.needs_reconnect ? (
            <Banner
              tone="danger"
              title="The cloud needs you to sign in again"
              actions={
                <Button size="sm" variant="primary" onClick={() => navigate({ name: "settings" })}>
                  Open settings
                </Button>
              }
            >
              Saved snapshots are safe on this computer and upload once you are connected again.
            </Banner>
          ) : null}
          {storage?.problem && !storage.needs_reconnect ? (
            <Banner tone="warn" title="Problem with the cloud connection">
              {storage.problem}
            </Banner>
          ) : null}

          {sorted.length === 0 ? (
            <div className="card">
              <EmptyState
                title="No instances are synced yet"
                action={
                  unsynced.length === 0 ? (
                    <Button onClick={() => setManual(true)}>Choose an instance folder</Button>
                  ) : undefined
                }
              >
                {unsynced.length > 0
                  ? "Prism instances were found on this computer. Pick one below to start."
                  : "PackSync did not find a Prism instance on its own. Choose an instance folder by hand."}
              </EmptyState>
            </div>
          ) : (
            <div className="instance-grid">
              {sorted.map((i) => (
                <InstanceCard key={i.id} inst={i} />
              ))}
            </div>
          )}

          <section className="col" aria-label="Statistics">
            <div className="section-head">
              <h2>Statistics</h2>
              <a className="section-link" href={href({ name: "activity" })}>
                Activity →
              </a>
            </div>
            <div className="stats-strip">
              <div className="stat-cell">
                <span className="stat-label">Synced Packs</span>
                <span className="stat-value">{synced.length}</span>
                <span className={`stat-sub ${attentionCount > 0 ? "warn" : "ok"}`}>
                  {attentionCount > 0 ? `${attentionCount} need review` : "All healthy"}
                </span>
              </div>
              <div className="stat-cell">
                <span className="stat-label">World &amp; Mod Data</span>
                <span className="stat-value">{formatBytes(totalBytes)}</span>
                <span className="stat-sub">
                  {plural(totalFiles, "file")} · {plural(totalWorlds, "world")}
                </span>
              </div>
              <div className="stat-cell">
                <span className="stat-label">Cloud Quota</span>
                <span className="stat-value">
                  {activeUsage?.quota ? formatBytes(activeUsage.quota.used) : "—"}
                </span>
                <span className="stat-sub">
                  {activeUsage?.quota?.total
                    ? `of ${formatBytes(activeUsage.quota.total)}`
                    : storage?.connected
                      ? "Cloud active"
                      : "Not connected"}
                </span>
              </div>
              <div className="stat-cell">
                <span className="stat-label">Upload Queue</span>
                <span className="stat-value">{pendingSnapshots}</span>
                <span className={`stat-sub ${pendingSnapshots > 0 ? "warn" : "ok"}`}>
                  {pendingBytes > 0 ? `${formatBytes(pendingBytes)} waiting` : "Queue clear"}
                </span>
              </div>
            </div>
          </section>

          {unsynced.length > 0 ? (
            <section className="col" aria-label="Instances that are not synced">
              <h2>Not syncing yet</h2>
              <div className="card flush">
                <ul className="list">
                  {unsynced.map((d) => (
                    <li key={d.root} className="row">
                      <div className="grow">
                        <strong>{d.name}</strong>
                        <div className="meta">
                          {packLine(d.identity)} · {worldsLine(d.worlds)}
                        </div>
                      </div>
                      <Button onClick={() => setEnableRoot(d.root)}>Start syncing…</Button>
                    </li>
                  ))}
                </ul>
              </div>
            </section>
          ) : null}
        </div>

        <aside className="dashboard-rail" aria-label="Overview">
          <ConnectionCard />
          <section className="card quick-cta-card" aria-label="Cloud quick action">
            <div className="quick-cta-art" aria-hidden="true">
              <Icon name="cloud" size={26} />
              <Icon name="shield" size={22} />
            </div>
            <div className="col" style={{ gap: 4 }}>
              <strong>{storage?.connected ? "Your PackSync vault" : "Connect cloud storage"}</strong>
              <p className="muted" style={{ fontSize: 13 }}>
                {storage?.connected
                  ? "Inspect cloud instances, quota, and multi-computer snapshots."
                  : "Sign in with Minecraft or pick a cloud folder to sync across computers."}
              </p>
            </div>
            {storage?.connected ? (
              <a className="btn primary" href={href({ name: "cloud" })}>
                Open Cloud →
              </a>
            ) : (
              <Button variant="primary" onClick={() => navigate({ name: "setup" })}>
                Connect cloud →
              </Button>
            )}
          </section>
        </aside>
      </div>

      {manual ? (
        <ManualInstanceDialog
          onClose={() => setManual(false)}
          onChosen={(root) => {
            setManual(false);
            setEnableRoot(root);
          }}
        />
      ) : null}
      {enableRoot ? (
        <EnableDialog
          root={enableRoot}
          onClose={() => setEnableRoot(null)}
          onEnabled={(id) => {
            setEnableRoot(null);
            void store.refreshDiscovered().catch(() => {});
            navigate({ name: "instance", id, tab: null });
          }}
        />
      ) : null}
    </div>
  );
}
