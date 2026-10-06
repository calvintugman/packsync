import { useEffect, useState } from "react";
import { EnableDialog } from "../dialogs/EnableDialog";
import { ManualInstanceDialog } from "../dialogs/ManualInstanceDialog";
import { packLine, worldsLine } from "../lib/format";
import { useAppState, useStore } from "../state/context";
import { Button, EmptyState, Pill } from "../ui/controls";

/** The discovered-instances list used by the setup wizard. */
export function InstanceCardLite() {
  const store = useStore();
  const discovered = useAppState((s) => s.discovered);
  const [root, setRoot] = useState<string | null>(null);
  const [manual, setManual] = useState(false);
  useEffect(() => {
    void store.refreshDiscovered().catch(() => {});
  }, [store]);
  const list = discovered ?? [];
  return (
    <div className="col">
      {list.length === 0 ? (
        <div className="card">
          <EmptyState title="No Prism instances found">Prism's data folder was not found, or it has no instances yet. Choose an instance folder by hand.</EmptyState>
        </div>
      ) : (
        <div className="card flush">
          <ul className="list" aria-label="Prism instances">
            {list.map((d) => (
              <li key={d.root} className="row">
                <div className="grow">
                  <strong>{d.name}</strong>
                  <div className="meta">
                    {packLine(d.identity)} · {worldsLine(d.worlds)}
                  </div>
                </div>
                {d.sync_enabled ? (
                  <Pill tone="ok">Syncing</Pill>
                ) : (
                  <Button onClick={() => setRoot(d.root)}>Preview and start…</Button>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
      <div className="row">
        <Button onClick={() => setManual(true)}>Choose a folder by hand…</Button>
      </div>
      {manual ? (
        <ManualInstanceDialog
          onClose={() => setManual(false)}
          onChosen={(r) => {
            setManual(false);
            setRoot(r);
          }}
        />
      ) : null}
      {root ? (
        <EnableDialog
          root={root}
          onClose={() => setRoot(null)}
          onEnabled={() => {
            setRoot(null);
            void store.refreshDiscovered().catch(() => {});
          }}
        />
      ) : null}
    </div>
  );
}
