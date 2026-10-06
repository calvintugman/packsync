import { useState } from "react";
import type { DiscoveredInstance } from "../api/types";
import { PathField } from "../components/PathField";
import { packLine, worldsLine } from "../lib/format";
import { useAction, useStore } from "../state/context";
import { Banner, Button } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Choose an instance folder by hand (anything Prism's own discovery missed). */
export function ManualInstanceDialog({ onClose, onChosen }: { onClose: () => void; onChosen: (root: string) => void }) {
  const store = useStore();
  const [path, setPath] = useState("");
  const [found, setFound] = useState<DiscoveredInstance | null>(null);
  const check = useAction(async () => {
    setFound(null);
    setFound(await store.call("launcher.describe", { launcher: "prism", path: path.trim() }));
  });
  return (
    <Dialog
      title="Choose an instance folder"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={!found} onClick={() => found && onChosen(found.root)}>
            Continue
          </Button>
        </>
      }
    >
      <p className="muted">
        Pick the instance's folder — the one that contains <code>instance.cfg</code>. In Prism: right-click the instance →
        “Folder”, then go one level up if you are inside <code>.minecraft</code>.
      </p>
      <PathField label="Instance folder" value={path} onChange={(v) => { setPath(v); setFound(null); }} browseTitle="Choose an instance folder" />
      <div className="row">
        <Button busy={check.busy} disabled={!path.trim()} onClick={() => void check.run()}>
          Check this folder
        </Button>
      </div>
      {check.error ? <ErrorNote error={check.error} /> : null}
      {found ? (
        <Banner tone="ok" title={found.name}>
          {packLine(found.identity)} · {worldsLine(found.worlds)}
          <div className="faint">{found.root}</div>
        </Banner>
      ) : null}
    </Dialog>
  );
}
