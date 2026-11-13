import { useId, useState } from "react";
import type { StorageStatus } from "../api/types";
import { useAction, useAppState, useStore } from "../state/context";
import { Banner, Button, TextArea, TextInput } from "../ui/controls";
import { ErrorNote } from "../ui/ErrorNote";
import { Icon } from "../ui/Icon";
import type { IconName } from "../ui/Icon";

/** A large, card-shaped choice. Its accessible name is the title alone; the text describes it. */
function OptionCard(props: {
  icon: IconName;
  title: string;
  text: string;
  primary?: boolean;
  busy?: boolean;
  onClick: () => void;
}) {
  const id = useId();
  return (
    <button
      type="button"
      className={`option-card ${props.primary ? "primary" : ""}`}
      aria-labelledby={`${id}-t`}
      aria-describedby={`${id}-d`}
      aria-busy={props.busy || undefined}
      disabled={props.busy}
      onClick={props.onClick}
    >
      <span className="option-icon" aria-hidden="true">
        {props.busy ? (
          <span className="spinner" />
        ) : (
          <Icon name={props.icon} size={19} />
        )}
      </span>
      <span className="option-title" id={`${id}-t`}>
        {props.title}
      </span>
      <span className="option-text" id={`${id}-d`}>
        {props.text}
      </span>
    </button>
  );
}

type Step = "choose" | "join" | "recover";

/**
 * The PackSync cloud: nothing to sign up for. The first computer starts it; every other computer joins
 * with a short code shown on one that is already in. Everything is encrypted on the computer.
 */
export function HostedSetup({
  onConnected,
}: {
  onConnected?: (s: StorageStatus) => void;
}) {
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
    const status = await store.runJob("storage.hosted_join", {
      code: code.trim(),
    });
    await finish(status, "This computer joined your PackSync cloud.");
  });
  const recover = useAction(async () => {
    const status = await store.runJob("storage.hosted_recover", {
      key: recoveryText.trim(),
    });
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
  const addPasskey = useAction(async () => {
    await store.runJob("storage.passkey_add", undefined);
    store.toast(
      "success",
      "Passkey added. If you ever lose every computer, it gets you back in.",
    );
  });
  const passkeyRecover = useAction(async () => {
    const status = await store.runJob("storage.passkey_recover", undefined);
    await finish(status, "You're back in your PackSync cloud.");
  });
  const [showRecovery, setShowRecovery] = useState(false);
  const showKey = useAction(async () => {
    const r = await store.call("storage.hosted_recovery_key");
    setRecoveryKey(r.key);
  });

  if (storage?.kind === "hosted") {
    return (
      <div className="col gap-lg">
        <Banner tone="ok" title="Using the PackSync cloud">
          {storage.detail}. Everything is encrypted on this computer before it
          is uploaded; the service cannot read it.
        </Banner>
        <div className="card col">
          <h3>Add another computer</h3>
          <p className="muted">
            Install PackSync on the other computer, choose{" "}
            <strong>Join from another computer</strong>, and type the code shown
            here. The code works once, for 10 minutes.
          </p>
          {shownCode ? (
            <div className="col" aria-live="polite">
              <p className="pairing-code">{shownCode}</p>
              <p className="faint">Waiting for the other computer…</p>
            </div>
          ) : null}
          {joinedNote ? (
            <p>
              The other computer joined. It can now bring your instances over.
            </p>
          ) : null}
          {pair.error ? <ErrorNote error={pair.error} /> : null}
          <div className="row">
            <Button
              variant="primary"
              icon="cloud"
              busy={pair.busy}
              onClick={() => void pair.run()}
            >
              {shownCode ? "Waiting…" : "Show a pairing code"}
            </Button>
          </div>
        </div>
        <div className="card col">
          <h3>Passkey</h3>
          <p className="muted">
            If you ever lose every computer, a passkey gets you back in —
            nothing to write down. Use your phone, Windows Hello, or your
            browser's password manager. Adding two (say, phone and computer) is
            safest.
          </p>
          {addPasskey.busy ? (
            <p className="faint">Finish in your browser…</p>
          ) : null}
          {addPasskey.error ? <ErrorNote error={addPasskey.error} /> : null}
          <div className="row">
            <Button
              variant="primary"
              icon="shield"
              busy={addPasskey.busy}
              onClick={() => void addPasskey.run()}
            >
              Add a passkey
            </Button>
            {!showRecovery ? (
              <button
                type="button"
                className="link-btn"
                onClick={() => setShowRecovery(true)}
              >
                Use a recovery key instead
              </button>
            ) : null}
          </div>
        </div>
        {showRecovery ? (
          <div className="card col">
            <h3>Recovery key</h3>
            <p className="muted">
              For passkey apps that can't protect encrypted data: a text that
              gets you back in. Save it in a password manager or print it.
              Anyone who has it can read your PackSync data.
            </p>
            {recoveryKey ? (
              <TextArea
                label="Your recovery key"
                value={recoveryKey}
                readOnly
                rows={3}
                onFocus={(e) => e.currentTarget.select()}
              />
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
        ) : null}
      </div>
    );
  }

  return (
    <div className="col gap-lg">
      <p className="muted">
        <Icon name="shield" size={14} /> No account to make. Your worlds are
        encrypted on your computer before upload: the PackSync service stores
        them but cannot read them.
      </p>

      {step === "choose" ? (
        <div className="col gap-lg">
          <div className="option-grid">
            <OptionCard
              primary
              icon="cloud"
              title="Start on this computer"
              text="Your first computer. Your private, encrypted PackSync cloud is ready in a second."
              busy={start.busy}
              onClick={() => void start.run()}
            />
            <OptionCard
              icon="computer"
              title="Join from another computer"
              text="Already using PackSync elsewhere? Type the code that computer shows."
              onClick={() => setStep("join")}
            />
          </div>
          {start.error ? <ErrorNote error={start.error} /> : null}
          {passkeyRecover.busy ? (
            <p className="faint">Finish in your browser…</p>
          ) : null}
          {passkeyRecover.error ? (
            <ErrorNote error={passkeyRecover.error} />
          ) : null}
          <p className="faint">
            Lost every computer?{" "}
            <button
              type="button"
              className="link-btn"
              disabled={passkeyRecover.busy}
              onClick={() => void passkeyRecover.run()}
            >
              Sign in with a passkey
            </button>{" "}
            or{" "}
            <button
              type="button"
              className="link-btn"
              onClick={() => setStep("recover")}
            >
              use a recovery key
            </button>
          </p>
        </div>
      ) : null}

      {step === "join" ? (
        <div className="col">
          <p className="muted">
            On a computer that already uses PackSync, open Settings → Storage
            and choose <strong>Show a pairing code</strong>.
          </p>
          <TextInput
            label="Pairing code"
            value={code}
            onChange={(e) => setCode(e.currentTarget.value)}
            placeholder="4821-K7QM-X2PD"
            autoComplete="off"
            spellCheck={false}
          />
          {join.busy ? (
            <p className="faint">Connecting to the other computer…</p>
          ) : null}
          {join.error ? <ErrorNote error={join.error} /> : null}
          <div className="row">
            <Button
              variant="primary"
              busy={join.busy}
              disabled={code.replace(/[\s-]/g, "").length !== 12}
              onClick={() => void join.run()}
            >
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
            <Button
              variant="primary"
              busy={recover.busy}
              disabled={!recoveryText.trim().startsWith("packsync1-")}
              onClick={() => void recover.run()}
            >
              Use this recovery key
            </Button>
            <Button onClick={() => setStep("choose")}>Back</Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
