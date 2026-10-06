import { useEffect, useRef, useState } from "react";
import type { HistoryEntry, InstanceStatus } from "../api/types";
import { JobBar } from "../components/JobBar";
import { formatBytes, formatDateTime, formatRelative, plural } from "../lib/format";
import { useAction, useStore } from "../state/context";
import { Banner, Button, Choice } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

type Pick = "promote" | "discard";

/** The last session did not end normally. PackSync quarantined what it found; the user decides. */
export function CrashDialog({ instance, onClose }: { instance: InstanceStatus; onClose: () => void }) {
  const store = useStore();
  const [crash, setCrash] = useState<HistoryEntry | null>(null);
  const [pick, setPick] = useState<Pick>("promote");
  const [jobId, setJobId] = useState<string | null>(null);

  const load = useAction(async () => {
    const h = await store.call("instance.history", { instance: instance.id, online: false });
    setCrash(h.find((e) => e.kind === "crash" && e.is_base) ?? h.find((e) => e.kind === "crash") ?? null);
  });
  const decide = useAction(async () => {
    if (pick === "promote") {
      await store.runJob("crash.promote", { instance: instance.id }, { onStart: setJobId });
      store.toast("success", "Kept. This is now your latest progress.");
    } else {
      await store.runJob("crash.discard", { instance: instance.id }, { onStart: setJobId });
      store.toast("success", "Went back to the last good save. The crash save stays in History.");
    }
    await store.refreshInstances();
    onClose();
  });

  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void load.run();
  }, [load]);

  return (
    <Dialog
      title={`${instance.name} did not close normally`}
      onClose={onClose}
      locked={decide.busy}
      footer={
        <>
          <Button onClick={onClose} disabled={decide.busy}>
            Decide later
          </Button>
          <Button variant="primary" busy={decide.busy} onClick={() => void decide.run()}>
            Apply this choice
          </Button>
        </>
      }
    >
      <p>
        The game crashed or was stopped abruptly (a power cut, a killed process). PackSync saved what was on disk as a
        <strong> crash save</strong> and does not sync it automatically — a crash can leave a world half-written.
      </p>
      {crash ? (
        <div className="card">
          <div className="row">
            <strong>Crash save</strong>
            <span className="muted">
              {formatRelative(crash.created)} · {plural(crash.files, "file")} · {formatBytes(crash.bytes)}
            </span>
          </div>
          <div className="faint">{formatDateTime(crash.created)}{crash.outcome ? ` · ${crash.outcome}` : ""}</div>
        </div>
      ) : null}
      {instance.state_detail ? <Banner tone="warn">{instance.state_detail}</Banner> : null}
      <fieldset className="col" style={{ border: 0, padding: 0, margin: 0 }}>
        <legend className="sr-only">What should happen</legend>
        <Choice name="crash" value="promote" selected={pick === "promote"} onSelect={() => setPick("promote")} title="Keep it — continue from here">
          Use what is on disk now as your latest progress. Open the world first if you want to check it is fine.
        </Choice>
        <Choice name="crash" value="discard" selected={pick === "discard"} onSelect={() => setPick("discard")} title="Go back to the last good save">
          Restore the last snapshot from before the session. The crash save stays in History and in your recovery points.
        </Choice>
      </fieldset>
      {load.error ? <ErrorNote error={load.error} /> : null}
      {decide.error ? <ErrorNote error={decide.error} instanceId={instance.id} /> : null}
      {decide.busy ? <JobBar jobId={jobId} /> : null}
    </Dialog>
  );
}
