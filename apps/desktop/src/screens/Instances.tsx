import { useEffect, useState } from "react";
import { InstanceCard } from "../components/InstanceCard";
import { EnableDialog } from "../dialogs/EnableDialog";
import { ManualInstanceDialog } from "../dialogs/ManualInstanceDialog";
import { packLine, worldsLine } from "../lib/format";
import { needsAttention } from "../lib/phases";
import { navigate } from "../lib/router";
import { useAppState, useStore } from "../state/context";
import { Banner, Button, EmptyState } from "../ui/controls";

export function InstancesScreen() {
  const store = useStore();
  const instances = useAppState((s) => s.instances);
  const storage = useAppState((s) => s.storage);
  const discovered = useAppState((s) => s.discovered);
  const [enableRoot, setEnableRoot] = useState<string | null>(null);
  const [manual, setManual] = useState(false);

  useEffect(() => {
    void store.refreshDiscovered().catch(() => {});
  }, [store]);

  const synced = instances.filter((i) => i.sync_enabled);
  const sorted = [...synced].sort((a, b) => Number(needsAttention(b)) - Number(needsAttention(a)) || a.name.localeCompare(b.name));
  const unsynced = (discovered ?? []).filter((d) => !d.sync_enabled);

  return (
    <div className="page">
      <div className="page-head">
        <h1>Instances</h1>
        <Button icon="plus" onClick={() => setManual(true)}>
          Add a folder…
        </Button>
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
        <div className="col gap-lg">
          {sorted.map((i) => (
            <InstanceCard key={i.id} inst={i} />
          ))}
        </div>
      )}

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
