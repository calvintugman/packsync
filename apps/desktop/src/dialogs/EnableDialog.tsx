import { useEffect, useRef, useState } from "react";
import type { InstanceId, PreviewReport } from "../api/types";
import { JobBar } from "../components/JobBar";
import { PreviewView } from "../components/PreviewView";
import { asDecisions, proposals, UnknownChoices } from "../components/UnknownChoices";
import type { Choices } from "../components/UnknownChoices";
import { describeError } from "../api/errors";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, Checkbox, TextArea } from "../ui/controls";
import { Dialog } from "../ui/Dialog";
import { ErrorNote } from "../ui/ErrorNote";

/** Preview what an instance would sync, then start syncing it. */
export function EnableDialog({
  root,
  onClose,
  onEnabled,
}: {
  root: string;
  onClose: () => void;
  onEnabled: (id: InstanceId) => void;
}) {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const [preview, setPreview] = useState<PreviewReport | null>(null);
  const [rules, setRules] = useState("");
  const [showRules, setShowRules] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);
  const [withHooks, setWithHooks] = useState(true);
  const [choices, setChoices] = useState<Choices>({});

  const load = useAction(async (overrides: string) => {
    const r = await store.runJob("instances.preview", { root, launcher: "prism", rules_overrides: overrides }, { onStart: setJobId });
    setPreview(r);
    setChoices((c) => ({ ...proposals(r.suggestions), ...c }));
  });
  const enable = useAction(async () => {
    const res = await store.runJob(
      "instances.enable",
      { path: root, launcher: "prism", rules_overrides: rules, decisions: asDecisions(choices), upload: Boolean(storage?.connected) },
      { onStart: setJobId },
    );
    if (res.upload_error) {
      store.toast("warn", describeError(res.upload_error).message, "Saved here, not uploaded yet");
    } else {
      store.toast("success", "Syncing is on. The first snapshot is saved.");
    }
    if (withHooks) {
      // Prism's pre-launch / post-exit commands, so pressing Play in Prism syncs (instance.cfg is backed up)
      const instance = res.outcome.instance;
      try {
        const plan = await store.call("hooks.plan_install", { instance });
        if (plan.blockers.length > 0) {
          store.toast("warn", plan.blockers.join(" "), "Prism integration not installed yet — see the instance's Hooks tab");
        } else {
          await store.call("hooks.apply", { instance, plan });
          store.toast("success", "Pressing Play in Prism now syncs this instance.");
        }
      } catch (e) {
        store.toast("warn", describeError(e).message, "Prism integration not installed — see the instance's Hooks tab");
      }
    }
    await store.refreshInstances();
    onEnabled(res.outcome.instance);
  });

  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void load.run("");
  }, [load]);

  const busy = load.busy || enable.busy;
  return (
    <Dialog
      title="Start syncing this instance"
      onClose={onClose}
      wide
      locked={enable.busy}
      footer={
        <>
          <Button onClick={onClose} disabled={enable.busy}>
            Cancel
          </Button>
          <Button variant="primary" busy={enable.busy} disabled={!preview || load.busy} onClick={() => void enable.run()}>
            Start syncing
          </Button>
        </>
      }
    >
      <p className="muted">
        PackSync syncs game state — mods, configuration and worlds — and leaves out logs, caches, crash reports and
        anything specific to this computer. Nothing is uploaded until you confirm.
      </p>
      {storage && !storage.connected ? (
        <Banner tone="info" title="No cloud is connected yet">
          The first snapshot is saved on this computer. It is uploaded as soon as you connect your cloud.
        </Banner>
      ) : null}
      {busy ? <JobBar jobId={jobId} /> : null}
      {load.error ? <ErrorNote error={load.error} /> : null}
      {enable.error ? <ErrorNote error={enable.error} /> : null}
      {preview ? <PreviewView report={preview} /> : null}
      {preview ? <UnknownChoices suggestions={preview.suggestions} value={choices} onChange={setChoices} /> : null}
      {preview ? (
        <Checkbox
          checked={withHooks}
          onChange={setWithHooks}
          disabled={enable.busy}
          hint="Adds PackSync's pre-launch and post-exit commands to this instance in Prism (your own commands are kept; instance.cfg is backed up; removable from the Hooks tab). Prism must be closed."
        >
          Sync automatically when I press Play in Prism
        </Checkbox>
      ) : null}
      {preview ? (
        <div className="col">
          <div className="row">
            <Button size="sm" variant="ghost" onClick={() => setShowRules((v) => !v)} aria-expanded={showRules}>
              {showRules ? "Hide custom rules" : "Custom rules (advanced)"}
            </Button>
          </div>
          {showRules ? (
            <>
              <TextArea
                label="Extra rules (TOML)"
                hint="Appended after the built-in rules. Example: include screenshots with a rule that matches $GAME/screenshots/**. See docs/rules.md."
                value={rules}
                spellCheck={false}
                onChange={(e) => setRules(e.target.value)}
                placeholder={'version = 1\n\n[[rule]]\nid = "keep-screenshots"\nmatch = ["$GAME/screenshots/**"]\naction = "include"\ncategory = "screenshot"\nreason = "I want them everywhere."'}
              />
              <div className="row">
                <Button size="sm" busy={load.busy} onClick={() => void load.run(rules)}>
                  Check the rules
                </Button>
              </div>
            </>
          ) : null}
        </div>
      ) : null}
    </Dialog>
  );
}
