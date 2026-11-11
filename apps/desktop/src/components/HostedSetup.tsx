import { useState } from "react";
import type { StorageStatus } from "../api/types";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, TextArea, TextInput } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";

type Step = "choose" | "join" | "recover";

/**
 * The PackSync cloud: nothing to sign up for. The first computer starts it; every other computer joins
 * with a short code shown on one that is already in. Everything is encrypted on the computer.
 */
export function HostedSetup({ onConnected }: { onConnected?: (s: StorageStatus) => void }) {
  const store = useStore();
  const storage = useAppState((s) => s.storage);
  const [step, setStep] = useState<Step>("choose");
  const [code, setCode] = useState("");
  const [recoveryText, setRecoveryText] = useState("");
  const [shownCode, setShownCode] = useState<string | null>(null);
  const [joinedNote, setJoinedNote] = useState(false);
  const [recoveryKey, setRecoveryKey] = useState<string | null>(null);

  const finish = async (status: StorageStatus, message: string) => {
    await store.refreshSettings();
    await store.refreshStorage();
    store.toast("success", message);
    onConnected?.(status);
  };

  const start = useAction(async () => {
    const status = await store.runJob("storage.hosted_start", undefined);
    await finish(status, "Your PackSync cloud is ready.");
  });
  const join = useAction(async () => {
    const status = await store.runJob("storage.hosted_join", { code: code.trim() });
    await finish(status, "This computer joined your PackSync cloud.");
  });
  const recover = useAction(async () => {
    const status = await store.runJob("storage.hosted_recover", { key: recoveryText.trim() });
    setRecoveryText("");
    await finish(status, "This computer uses your PackSync cloud again.");
  });
  const pair = useAction(async () => {
    setJoinedNote(false);
    const r = await store.call("storage.hosted_pair_start");
    setShownCode(r.code);
    try {
      await store.runJob("storage.hosted_pair_wait", undefined);
      setJoinedNote(true);
      store.toast("success", "The other computer joined.");
    } finally {
      setShownCode(null);
    }
  });
  const showKey = useAction(async () => {
    const r = await store.call("storage.hosted_recovery_key");
    setRecoveryKey(r.key);
  });

  if (storage?.kind === "hosted") {
    return (
      <div className="col gap-lg">
        <Banner tone="ok" title="Using the PackSync cloud">
          {storage.detail}. Everything is encrypted on this computer before it is uploaded; the service cannot read it.
        </Banner>
        <div className="card col">
          <h3>Add another computer</h3>
          <p className="muted">
            Install PackSync on the other computer, choose <strong>Join from another computer</strong>, and type the
            code shown here. The code works once, for 10 minutes.
          </p>
          {shownCode ? (
            <div className="col" aria-live="polite">
              <p className="pairing-code" style={{ fontSize: "2rem", fontFamily: "monospace", letterSpacing: "0.1em", margin: 0 }}>
                {shownCode}
              </p>
              <p className="faint">Waiting for the other computer…</p>
            </div>
          ) : null}
          {joinedNote ? <p>The other computer joined. It can now bring your instances over.</p> : null}
          {pair.error ? <ErrorNote error={pair.error} /> : null}
          <div className="row">
            <Button variant="primary" icon="cloud" busy={pair.busy} onClick={() => void pair.run()}>
              {shownCode ? "Waiting…" : "Show a pairing code"}
            </Button>
          </div>
        </div>
        <div className="card col">
          <h3>Recovery key</h3>
          <p className="muted">
            If every computer with PackSync is lost, the recovery key is the only way back to your worlds. Save it in a
            password manager or print it. Anyone who has it can read your PackSync data.
          </p>
          {recoveryKey ? (
            <TextArea label="Your recovery key" value={recoveryKey} readOnly rows={3} onFocus={(e) => e.currentTarget.select()} />
          ) : null}
          {showKey.error ? <ErrorNote error={showKey.error} /> : null}
          <div className="row">
            {recoveryKey ? (
              <Button onClick={() => setRecoveryKey(null)}>Hide it</Button>
            ) : (
              <Button busy={showKey.busy} onClick={() => void showKey.run()}>
                Show the recovery key
              </Button>
            )}
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="col gap-lg">
      <Banner tone="info" title="Private by design">
        No account to make, nothing to configure. Your worlds are encrypted on this computer before upload: the
        PackSync service stores them but cannot read them.
      </Banner>

      {step === "choose" ? (
        <div className="col">
          {start.error ? <ErrorNote error={start.error} /> : null}
          <div className="row">
            <Button variant="primary" icon="cloud" busy={start.busy} onClick={() => void start.run()}>
              Start on this computer
            </Button>
            <Button onClick={() => setStep("join")}>Join from another computer</Button>
          </div>
          <p className="faint">Start on your first computer. On the others, choose Join and type the code the first one shows.</p>
          <div className="row">
            <Button size="sm" onClick={() => setStep("recover")}>
              I have a recovery key
            </Button>
          </div>
        </div>
      ) : null}

      {step === "join" ? (
        <div className="col">
          <p className="muted">
            On a computer that already uses PackSync, open Settings → Storage and choose <strong>Show a pairing code</strong>.
          </p>
          <TextInput
            label="Pairing code"
            value={code}
            onChange={(e) => setCode(e.currentTarget.value)}
            placeholder="4821-K7QM-X2PD"
            autoComplete="off"
            spellCheck={false}
          />
          {join.busy ? <p className="faint">Connecting to the other computer…</p> : null}
          {join.error ? <ErrorNote error={join.error} /> : null}
          <div className="row">
            <Button variant="primary" busy={join.busy} disabled={code.replace(/[\s-]/g, "").length !== 12} onClick={() => void join.run()}>
              Join
            </Button>
            <Button onClick={() => setStep("choose")}>Back</Button>
          </div>
        </div>
      ) : null}

      {step === "recover" ? (
        <div className="col">
          <TextArea
            label="Recovery key"
            value={recoveryText}
            onChange={(e) => setRecoveryText(e.currentTarget.value)}
            placeholder="packsync1-…"
            rows={3}
            spellCheck={false}
          />
          {recover.error ? <ErrorNote error={recover.error} /> : null}
          <div className="row">
            <Button variant="primary" busy={recover.busy} disabled={!recoveryText.trim().startsWith("packsync1-")} onClick={() => void recover.run()}>
              Use this recovery key
            </Button>
            <Button onClick={() => setStep("choose")}>Back</Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
