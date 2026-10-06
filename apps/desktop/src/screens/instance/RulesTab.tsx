import { useState } from "react";
import type { InstanceStatus, RulesExplain } from "../../api/types";
import { useLoad } from "../../components/useLoad";
import { PreviewDialog } from "../../dialogs/PreviewDialog";
import { useAction, useStore } from "../../state/context";
import { Banner, Button, TextInput } from "../../ui/controls";
import { ErrorNote } from "../../ui/ErrorNote";

/** The rules that decide what is synced, and a way to ask "why is this file (not) synced?". */
export function RulesTab({ inst }: { inst: InstanceStatus }) {
  const store = useStore();
  const rules = useLoad(() => store.runJob("rules.show", { instance: inst.id }), inst.id);
  const [path, setPath] = useState("");
  const [answer, setAnswer] = useState<RulesExplain | null>(null);
  const [preview, setPreview] = useState(false);
  const ask = useAction(async () => {
    setAnswer(await store.runJob("rules.explain", { instance: inst.id, path: path.trim() }));
  });
  return (
    <div className="col">
      <p className="muted">
        PackSync syncs game state — mods, configuration and worlds — and leaves out logs, caches, crash reports and
        anything specific to one computer. Secrets such as launcher account files are never uploaded, whatever the rules say.
      </p>
      <div className="card col">
        <h3>Why is this file (not) synced?</h3>
        <form
          className="row"
          style={{ alignItems: "flex-end" }}
          onSubmit={(e) => {
            e.preventDefault();
            if (path.trim()) void ask.run();
          }}
        >
          <div className="grow">
            <TextInput label="Path inside the instance" placeholder=".minecraft/logs/latest.log" value={path} onChange={(e) => setPath(e.target.value)} spellCheck={false} />
          </div>
          <Button type="submit" busy={ask.busy} disabled={!path.trim()}>
            Explain
          </Button>
        </form>
        {ask.error ? <ErrorNote error={ask.error} /> : null}
        {answer ? (
          <Banner tone={answer.synced ? "ok" : "info"} title={answer.synced ? "Synced" : "Not synced"}>
            {answer.explanation}
          </Banner>
        ) : null}
      </div>
      <div className="row">
        <Button onClick={() => setPreview(true)}>Show everything that is synced…</Button>
      </div>
      <div className="card col">
        <h3>Rules in effect</h3>
        {rules.error ? <ErrorNote error={rules.error} /> : null}
        {rules.loading && !rules.data ? <span className="muted">Loading…</span> : null}
        {rules.data ? <pre className="code" tabIndex={0} aria-label="Rules in effect" style={{ maxHeight: 420 }}>{rules.data.rules}</pre> : null}
      </div>
      {preview ? <PreviewDialog root={inst.root} onClose={() => setPreview(false)} /> : null}
    </div>
  );
}
