import { useState } from "react";
import type { CloudInstance, RestoredInstance } from "../api/types";
import { JobBar } from "../components/JobBar";
import { PathField } from "../components/PathField";
import { formatBytes, formatDateTime, packLine } from "../lib/format";
import { navigate } from "../lib/router";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, Select, TextInput } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Bring an instance from the cloud to this computer as a new Prism instance. */
export function RestoreNewDialog({ cloud, onClose }: { cloud: CloudInstance; onClose: () => void }) {
  const store = useStore();
  const info = useAppState((s) => s.info);
  const dirs = info?.prism.data_dirs ?? [];
  const [dataDir, setDataDir] = useState(dirs[0] ?? "");
  const [folder, setFolder] = useState("");
  const [head, setHead] = useState(cloud.heads[0]?.id ?? "");
  const [minMem, setMinMem] = useState("");
  const [maxMem, setMaxMem] = useState("");
  const [javaPath, setJavaPath] = useState("");
  const [jobId, setJobId] = useState<string | null>(null);
  const [done, setDone] = useState<RestoredInstance | null>(null);

  const run = useAction(async () => {
    const mem = (v: string) => (v.trim() && Number.isFinite(Number(v)) ? Math.round(Number(v)) : null);
    const r = await store.runJob(
      "restore.new",
      {
        instance: cloud.id,
        snapshot: head || null,
        data_dir: dataDir.trim() || null,
        folder_name: folder.trim() || null,
        local: { min_memory_mb: mem(minMem), max_memory_mb: mem(maxMem), java_path: javaPath.trim() || null },
      },
      { onStart: setJobId },
    );
    setDone(r);
    store.toast("success", `“${cloud.name}” is on this computer.`);
    await store.refreshInstances();
  });

  return (
    <Dialog
      title={`Bring “${cloud.name}” to this computer`}
      onClose={onClose}
      locked={run.busy}
      footer={
        done ? (
          <>
            <Button onClick={onClose}>Close</Button>
            <Button
              variant="primary"
              onClick={() => {
                onClose();
                navigate({ name: "instance", id: done.instance, tab: null });
              }}
            >
              Open the instance
            </Button>
          </>
        ) : (
          <>
            <Button onClick={onClose} disabled={run.busy}>
              Cancel
            </Button>
            <Button variant="primary" busy={run.busy} disabled={!dataDir.trim()} onClick={() => void run.run()}>
              Bring it here
            </Button>
          </>
        )
      }
    >
      <p className="muted">
        {packLine(cloud.pack)} · {formatBytes(cloud.bytes)} · last saved {formatDateTime(cloud.last_updated)}
        {cloud.last_device ? ` on ${cloud.last_device}` : ""}. It is downloaded and checked in a hidden folder first,
        then appears in Prism all at once.
      </p>
      {cloud.heads.length > 1 ? (
        <Select label="Which version?" hint="The cloud has several versions because two computers saved separately." value={head} onChange={(e) => setHead(e.target.value)}>
          {cloud.heads.map((h, i) => (
            <option key={h.id} value={h.id}>
              {i + 1}. {h.device ?? "another computer"} — {formatDateTime(h.created)}
            </option>
          ))}
        </Select>
      ) : null}
      {dirs.length > 1 ? (
        <Select label="Prism installation" value={dataDir} onChange={(e) => setDataDir(e.target.value)}>
          {dirs.map((d) => (
            <option key={d} value={d}>
              {d}
            </option>
          ))}
        </Select>
      ) : (
        <PathField
          label="Prism data folder"
          value={dataDir}
          onChange={setDataDir}
          hint={dirs.length === 0 ? "Prism's data folder was not found. Start Prism once, or enter its location (the folder that contains “instances”)." : "Found automatically."}
          browseTitle="Choose Prism's data folder"
        />
      )}
      <TextInput label="Folder name (optional)" value={folder} onChange={(e) => setFolder(e.target.value)} hint="Leave empty to use the pack's name." />
      <details>
        <summary>
          <strong>Settings for this computer (optional)</strong>
        </summary>
        <div className="col" style={{ marginTop: 10 }}>
          <p className="muted">Memory and Java differ between computers and are never synced. Leave empty to use Prism's defaults.</p>
          <div className="row">
            <TextInput label="Minimum memory (MB)" inputMode="numeric" value={minMem} onChange={(e) => setMinMem(e.target.value)} />
            <TextInput label="Maximum memory (MB)" inputMode="numeric" value={maxMem} onChange={(e) => setMaxMem(e.target.value)} />
          </div>
          <PathField label="Java executable" value={javaPath} onChange={setJavaPath} placeholder="Use Prism's default" browseTitle="Choose the Java folder" />
        </div>
      </details>
      {run.busy ? <JobBar jobId={jobId} /> : null}
      {run.error ? <ErrorNote error={run.error} /> : null}
      {done ? (
        <Banner tone="ok" title="Done">
          Restored to <code>{done.root}</code>.
          {done.notes.length > 0 ? (
            <ul style={{ margin: "4px 0 0", paddingLeft: 18 }}>
              {done.notes.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          ) : null}
        </Banner>
      ) : null}
    </Dialog>
  );
}
