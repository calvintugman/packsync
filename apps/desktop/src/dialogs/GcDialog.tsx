import { useState } from "react";
import type { GcParams, GcPlan, GcReport } from "../api/types";
import { JobBar } from "../components/JobBar";
import { formatBytes, plural } from "../lib/format";
import { useAction, useStore } from "../state/context";
import { Banner, Button, Checkbox, TextInput } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Cloud cleanup: always a plan first, then exactly that plan — never automatic. */
export function GcDialog({ onClose }: { onClose: () => void }) {
  const store = useStore();
  const [retain, setRetain] = useState(false);
  const [keepSnapshots, setKeepSnapshots] = useState("20");
  const [keepDays, setKeepDays] = useState("30");
  const [plan, setPlan] = useState<GcPlan | null>(null);
  const [report, setReport] = useState<GcReport | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);

  const params = (): GcParams => ({
    grace_hours: 48,
    retention: retain
      ? { keep_snapshots: Math.max(1, Number(keepSnapshots) || 20), keep_days: Math.max(1, Number(keepDays) || 30), keep_crash_days: 90 }
      : null,
    instance: null,
    consolidate_indexes: true,
  });

  const makePlan = useAction(async () => {
    setReport(null);
    setPlan(await store.runJob("gc.plan", params(), { onStart: setJobId }));
  });
  const run = useAction(async () => {
    if (!plan) return;
    const r = await store.runJob("gc.run", { plan_id: plan.id, params: plan.params }, { onStart: setJobId });
    setReport(r);
    setPlan(null);
    store.toast("success", `Cleanup done: ${formatBytes(r.bytes_freed)} freed.`);
  });
  const empty = plan !== null && plan.delete.length === 0 && plan.expire.length === 0 && !plan.consolidate;

  return (
    <Dialog
      title="Clean up the cloud"
      onClose={onClose}
      wide
      locked={run.busy}
      footer={
        <>
          <Button onClick={onClose} disabled={run.busy}>
            Close
          </Button>
          <Button busy={makePlan.busy} disabled={run.busy} onClick={() => void makePlan.run()}>
            {plan ? "Plan again" : "Make a plan"}
          </Button>
          <Button variant="danger" busy={run.busy} disabled={!plan || empty} onClick={() => void run.run()}>
            Run this plan
          </Button>
        </>
      }
    >
      <p className="muted">
        Cleanup removes leftovers of interrupted uploads and, if you ask, releases the content of old snapshots.
        It only touches objects PackSync created, nothing younger than two days, and nothing a kept snapshot needs.
        You always see the plan first. Google Drive keeps deleted files in its trash for a while.
      </p>
      <Checkbox checked={retain} onChange={setRetain} hint="Heads and crash saves are always kept. Releasing a snapshot makes it impossible to restore, but keeps the history graph intact.">
        Also release old snapshots
      </Checkbox>
      {retain ? (
        <div className="row">
          <TextInput label="Keep the newest" inputMode="numeric" value={keepSnapshots} onChange={(e) => setKeepSnapshots(e.target.value)} />
          <TextInput label="…and everything from the last (days)" inputMode="numeric" value={keepDays} onChange={(e) => setKeepDays(e.target.value)} />
        </div>
      ) : null}
      {makePlan.busy || run.busy ? <JobBar jobId={jobId} /> : null}
      {makePlan.error ? <ErrorNote error={makePlan.error} /> : null}
      {run.error ? <ErrorNote error={run.error} /> : null}
      {empty ? <Banner tone="ok" title="Nothing to clean up">The cloud holds only what your snapshots need.</Banner> : null}
      {plan && !empty ? (
        <div className="col">
          <div className="stat-row">
            <div className="stat">
              <div className="num">{formatBytes(plan.total_delete_bytes)}</div>
              <div className="label">would be freed</div>
            </div>
            <div className="stat">
              <div className="num">{plan.delete.length}</div>
              <div className="label">objects deleted</div>
            </div>
            <div className="stat">
              <div className="num">{plan.expire.length}</div>
              <div className="label">snapshots released</div>
            </div>
          </div>
          {plan.consolidate ? <p>{plural(plan.consolidate.merge.length, "index object")} are merged into one.</p> : null}
          {plan.partially_live.packs > 0 ? (
            <p className="muted">
              {plural(plan.partially_live.packs, "pack")} still hold some needed data, so {formatBytes(plan.partially_live.reclaimable_bytes)} cannot be
              reclaimed yet.
            </p>
          ) : null}
          {plan.kept_unknown.length > 0 ? <p className="muted">{plural(plan.kept_unknown.length, "object")} PackSync did not create are left alone.</p> : null}
          {plan.notes.map((n) => (
            <p key={n} className="muted">
              {n}
            </p>
          ))}
          <details>
            <summary>
              <strong>What would be deleted</strong>
            </summary>
            <ul>
              {plan.delete.slice(0, 20).map((d) => (
                <li key={d.key}>
                  <code>{d.key}</code> · {formatBytes(d.size)} · {d.reason}
                </li>
              ))}
            </ul>
          </details>
          <p className="faint">Plan {plan.id}</p>
        </div>
      ) : null}
      {report ? (
        <Banner tone="ok" title="Cleanup finished">
          {formatBytes(report.bytes_freed)} freed: {plural(report.deleted_packs, "pack")} and {plural(report.deleted_indexes, "index object")} deleted,{" "}
          {plural(report.expired, "snapshot")} released.
        </Banner>
      ) : null}
    </Dialog>
  );
}
