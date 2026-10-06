import { useEffect, useRef, useState } from "react";
import type { HookPlan, InstanceId } from "../api/types";
import { useAction, useStore } from "../state/context";
import { Banner, Button } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Show exactly what installing (or removing) the launcher hooks changes, then apply it. */
export function HooksDialog({
  instance,
  mode,
  onClose,
  onApplied,
}: {
  instance: InstanceId;
  mode: "install" | "remove";
  onClose: () => void;
  onApplied: () => void;
}) {
  const store = useStore();
  const [plan, setPlan] = useState<HookPlan | null>(null);
  const load = useAction(async () => {
    setPlan(
      mode === "install"
        ? await store.call("hooks.plan_install", { instance })
        : await store.call("hooks.plan_remove", { instance }),
    );
  });
  const apply = useAction(async () => {
    if (!plan) return;
    await store.call("hooks.apply", { instance, plan });
    store.toast("success", mode === "install" ? "Launcher hooks installed." : "Launcher hooks removed.");
    onApplied();
    onClose();
  });
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void load.run();
  }, [load]);

  const blocked = (plan?.blockers.length ?? 0) > 0;
  const nothing = plan !== null && plan.changes.length === 0;
  return (
    <Dialog
      title={mode === "install" ? "Install launcher hooks" : "Remove launcher hooks"}
      onClose={onClose}
      wide
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" busy={apply.busy} disabled={!plan || blocked || nothing} onClick={() => void apply.run()}>
            {mode === "install" ? "Install hooks" : "Remove hooks"}
          </Button>
        </>
      }
    >
      <p className="muted">
        {mode === "install"
          ? "With hooks, starting this instance from Prism itself syncs first and saves and uploads when the game closes. This changes only this instance's settings in Prism, and you can remove it again at any time."
          : "This puts back the commands Prism had for this instance before PackSync's hooks were installed."}
      </p>
      {load.error ? <ErrorNote error={load.error} /> : null}
      {plan && blocked ? (
        <Banner tone="danger" title="Not possible right now">
          <ul style={{ margin: 0, paddingLeft: 18 }}>
            {plan.blockers.map((b) => (
              <li key={b}>{b}</li>
            ))}
          </ul>
        </Banner>
      ) : null}
      {nothing ? <Banner tone="info">There is nothing to change.</Banner> : null}
      {plan?.preserved_commands.length ? (
        <Banner tone="info" title="Your own commands keep running">
          They run right after PackSync's part:
          <ul style={{ margin: "4px 0 0", paddingLeft: 18 }}>
            {plan.preserved_commands.map((c) => (
              <li key={c}>
                <code>{c}</code>
              </li>
            ))}
          </ul>
        </Banner>
      ) : null}
      {plan?.changes.map((c) => (
        <div key={c.key} className="col" style={{ gap: 4 }}>
          <div>
            <strong>{c.key}</strong> <span className="muted">in</span> <code>{c.file}</code>
          </div>
          <pre className="code diff-before" tabIndex={0} aria-label={`${c.key} before`}>
            {c.before === null ? "(not set)" : c.before}
          </pre>
          <pre className="code diff-after" tabIndex={0} aria-label={`${c.key} after`}>
            {c.after === null ? "(removed)" : c.after}
          </pre>
        </div>
      ))}
      {apply.error ? <ErrorNote error={apply.error} /> : null}
    </Dialog>
  );
}
