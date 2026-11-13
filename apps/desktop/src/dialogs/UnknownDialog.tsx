import { useState } from "react";
import type { InstanceStatus } from "../api/types";
import { asDecisions, proposals, UnknownChoices } from "../components/UnknownChoices";
import type { Choices } from "../components/UnknownChoices";
import { useAction, useStore } from "../state/context";
import { Button } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Decide about content of an instance that no rule covers; the answers are applied by a sync right away. */
export function UnknownDialog({ inst, onClose }: { inst: InstanceStatus; onClose: () => void }) {
  const store = useStore();
  const [choices, setChoices] = useState<Choices>(() => proposals(inst.unknown));
  const save = useAction(async () => {
    await store.call("instance.decide_unknown", { instance: inst.id, decisions: asDecisions(choices) });
    const synced = Object.values(choices).filter(Boolean).length;
    // the sync applies the answers (after catching up with the cloud); a failure is shown here
    await store.runJob("instance.sync", { instance: inst.id });
    store.toast("success", synced > 0 ? `Now syncing ${synced} more ${synced === 1 ? "item" : "items"}.` : "Saved your choices.", inst.name);
    await store.refreshInstances();
    onClose();
  });
  return (
    <Dialog
      title={`New mod data in ${inst.name}`}
      onClose={onClose}
      locked={save.busy}
      footer={
        <>
          <Button onClick={onClose} disabled={save.busy}>
            Later
          </Button>
          <Button variant="primary" busy={save.busy} onClick={() => void save.run()}>
            Save and sync
          </Button>
        </>
      }
    >
      <UnknownChoices suggestions={inst.unknown} value={choices} onChange={setChoices} />
      {save.error ? <ErrorNote error={save.error} /> : null}
    </Dialog>
  );
}
