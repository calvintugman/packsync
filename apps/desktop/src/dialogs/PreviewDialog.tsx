import { useEffect, useRef, useState } from "react";
import type { PreviewReport } from "../api/types";
import { JobBar } from "../components/JobBar";
import { PreviewView } from "../components/PreviewView";
import { useAction, useStore } from "../state/context";
import { Button } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** What an already-synced instance syncs and what it leaves out (read-only). */
export function PreviewDialog({ root, onClose }: { root: string; onClose: () => void }) {
  const store = useStore();
  const [report, setReport] = useState<PreviewReport | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);
  const load = useAction(async () => {
    setReport(await store.runJob("instances.preview", { root, launcher: "prism" }, { onStart: setJobId }));
  });
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void load.run();
  }, [load]);
  return (
    <Dialog title="What is synced" onClose={onClose} wide footer={<Button onClick={onClose}>Close</Button>}>
      {load.busy ? <JobBar jobId={jobId} /> : null}
      {load.error ? <ErrorNote error={load.error} /> : null}
      {report ? <PreviewView report={report} /> : null}
    </Dialog>
  );
}
