import { useState } from "react";
import type { InstanceStatus } from "../../api/types";
import { useLoad } from "../../components/useLoad";
import { HooksDialog } from "../../dialogs/HooksDialog";
import { useStore } from "../../state/context";
import { Banner, Button, Kv, Pill } from "../../ui/controls";
import { ErrorNote } from "../../ui/ErrorNote";

export function HooksTab({ inst }: { inst: InstanceStatus }) {
  const store = useStore();
  const state = useLoad(() => store.call("hooks.state", { instance: inst.id }), inst.id);
  const [dialog, setDialog] = useState<"install" | "remove" | null>(null);
  const s = state.data;
  return (
    <div className="col">
      <p className="muted">
        Hooks are optional. Without them, use <strong>Play</strong> in PackSync: it syncs, starts Prism, and saves and
        uploads when the game closes. With hooks, starting the instance from <em>Prism itself</em> does the same.
      </p>
      {state.error ? <ErrorNote error={state.error} /> : null}
      <div className="card col">
        <div className="row">
          <h3 className="grow">Launcher hooks</h3>
          {s ? <Pill tone={s.installed ? (s.intact ? "ok" : "warn") : "neutral"}>{s.installed ? (s.intact ? "Installed" : "Changed since installed") : "Not installed"}</Pill> : null}
        </div>
        {s?.installed && !s.intact ? (
          <Banner tone="warn" title="Prism's settings for this instance no longer match what PackSync wrote">
            Something changed the commands after PackSync installed them. Reinstall to repair, or remove them.
          </Banner>
        ) : null}
        {s?.installed ? (
          <Kv
            rows={[
              ["Before the game starts", <code key="p">{s.pre_launch ?? "—"}</code>],
              ["After the game closes", <code key="e">{s.post_exit ?? "—"}</code>],
            ]}
          />
        ) : null}
        <div className="row">
          {!s?.installed || !s.intact ? (
            <Button variant="primary" onClick={() => setDialog("install")}>
              {s?.installed ? "Repair hooks…" : "Install hooks…"}
            </Button>
          ) : null}
          {s?.installed ? (
            <Button onClick={() => setDialog("remove")}>Remove hooks…</Button>
          ) : null}
        </div>
      </div>
      <div className="card col">
        <h3>What the hooks do</h3>
        <ul style={{ margin: 0, paddingLeft: 18 }}>
          <li>Before the game starts: fetch a newer version from the cloud (when this computer has no unsaved changes) and refuse to start if two versions need your decision.</li>
          <li>After a normal exit: save a snapshot and upload it in the background.</li>
          <li>If the game crashes: nothing is synced automatically. The next start asks you what to do with the crash save.</li>
          <li>Your own pre-launch and post-exit commands keep running.</li>
        </ul>
      </div>
      {dialog ? <HooksDialog instance={inst.id} mode={dialog} onClose={() => setDialog(null)} onApplied={state.reload} /> : null}
    </div>
  );
}
