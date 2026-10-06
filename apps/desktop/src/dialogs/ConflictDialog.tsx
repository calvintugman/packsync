import { useEffect, useRef, useState } from "react";
import type { ConflictDetails, InstanceStatus, Resolution } from "../api/types";
import { JobBar } from "../components/JobBar";
import { VersionCard } from "../components/VersionCard";
import { formatDateTime } from "../lib/format";
import { useAction, useStore } from "../state/context";
import { Banner, Button, Choice, Select, TextInput } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

type Pick = "keep_local" | "keep_cloud" | "split_local";

/** Two versions of one instance exist. PackSync never merges worlds: the user chooses, nothing is deleted. */
export function ConflictDialog({ instance, onClose }: { instance: InstanceStatus; onClose: () => void }) {
  const store = useStore();
  const [details, setDetails] = useState<ConflictDetails | null>(null);
  const [resolved, setResolved] = useState(false);
  const [pick, setPick] = useState<Pick>("keep_local");
  const [head, setHead] = useState("");
  const [name, setName] = useState(`${instance.name} (this computer)`);
  const [jobId, setJobId] = useState<string | null>(null);

  const load = useAction(async () => {
    const r = await store.runJob("conflict.show", { instance: instance.id }, { onStart: setJobId });
    const rep = r.report;
    if (r.conflict && rep.outcome === "conflict") {
      setDetails(rep);
      const first = rep.cloud[0];
      if (rep.cloud.length > 1 && first) setHead((h) => h || first.id);
    } else {
      setResolved(true);
    }
  });
  const resolve = useAction(async () => {
    const resolution: Resolution =
      pick === "keep_local"
        ? { choice: "keep_local" }
        : pick === "keep_cloud"
          ? { choice: "keep_cloud", head: head || null }
          : { choice: "split_local", new_name: name.trim() };
    const r = await store.runJob("conflict.resolve", { instance: instance.id, resolution }, { onStart: setJobId });
    store.toast(
      "success",
      pick === "split_local"
        ? `Your version continues as “${name.trim()}”. The original keeps the cloud's version.`
        : "Conflict resolved. The other version stays in History.",
    );
    if (!r.uploaded) store.toast("warn", "The result is saved here and will upload when the cloud is reachable.");
    await store.refreshInstances();
    onClose();
  });

  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void load.run();
  }, [load]);

  const several = (details?.cloud.length ?? 0) > 1;
  const nameOk = pick !== "split_local" || name.trim().length > 0;
  return (
    <Dialog
      title={`Two versions of ${instance.name}`}
      onClose={onClose}
      wide
      locked={resolve.busy}
      footer={
        <>
          <Button onClick={onClose} disabled={resolve.busy}>
            {resolved ? "Close" : "Decide later"}
          </Button>
          {!resolved ? (
            <Button variant="primary" busy={resolve.busy} disabled={!details || !nameOk} onClick={() => void resolve.run()}>
              Apply this choice
            </Button>
          ) : null}
        </>
      }
    >
      {load.busy ? <JobBar jobId={jobId} cancellable={false} /> : null}
      {load.error ? <ErrorNote error={load.error} /> : null}
      {resolved ? <Banner tone="ok" title="There is no conflict any more.">The instance is back in step with the cloud.</Banner> : null}
      {details ? (
        <>
          <p>
            {details.kind === "multiple_cloud_heads"
              ? "Two computers saved progress independently, so the cloud holds more than one version."
              : "This computer and the cloud both changed since they last agreed."}{" "}
            PackSync never merges worlds automatically — choose which one to continue with. <strong>Nothing is deleted:</strong>{" "}
            the other version stays in History.
          </p>
          <div className="row wrap" style={{ alignItems: "stretch" }}>
            <div className="grow" style={{ minWidth: 260 }}>
              <VersionCard
                title="This computer"
                v={details.local}
                note={details.local_dirty ? "It has changes that are not saved yet. They are saved first, so nothing is lost." : undefined}
              />
            </div>
            {details.cloud.map((c, i) => (
              <div className="grow" style={{ minWidth: 260 }} key={c.id}>
                <VersionCard title={details.cloud.length > 1 ? `In the cloud (${i + 1} of ${details.cloud.length})` : "In the cloud"} v={c} />
              </div>
            ))}
          </div>
          <fieldset className="col" style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className="sr-only">What should happen</legend>
            <Choice name="resolution" value="keep_local" selected={pick === "keep_local"} onSelect={() => setPick("keep_local")} title="Keep this computer's version">
              The cloud continues from this version. The other version stays in History and can be restored later.
            </Choice>
            <Choice name="resolution" value="keep_cloud" selected={pick === "keep_cloud"} onSelect={() => setPick("keep_cloud")} title="Use the cloud's version">
              This computer's version is saved in History first, then replaced by the cloud's. Recent changes are not lost.
            </Choice>
            {pick === "keep_cloud" && several ? (
              <div style={{ marginLeft: 34 }}>
                <Select label="Which cloud version?" value={head} onChange={(e) => setHead(e.target.value)}>
                  {details.cloud.map((c, i) => (
                    <option key={c.id} value={c.id}>
                      {i + 1}. {c.device ?? "another computer"} — {formatDateTime(c.created)}
                    </option>
                  ))}
                </Select>
              </div>
            ) : null}
            <Choice name="resolution" value="split_local" selected={pick === "split_local"} onSelect={() => setPick("split_local")} title="Keep both, as separate instances">
              This computer's version becomes its own instance with its own cloud copy. The original keeps the cloud's
              version — bring it back here from the Cloud page whenever you like.
            </Choice>
            {pick === "split_local" ? (
              <div style={{ marginLeft: 34 }}>
                <TextInput label="Name of the separate instance" value={name} onChange={(e) => setName(e.target.value)} />
              </div>
            ) : null}
          </fieldset>
          {resolve.error ? <ErrorNote error={resolve.error} /> : null}
          {resolve.busy ? <JobBar jobId={jobId} /> : null}
        </>
      ) : null}
    </Dialog>
  );
}
