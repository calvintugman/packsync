import { useState } from "react";
import { InstanceCardLite } from "../components/InstanceCardLite";
import { StorageSetup } from "../components/StorageSetup";
import { Banner, Button } from "../ui/controls";
import { navigate } from "../lib/router";
import { useAppState } from "../state/context";
import { Icon } from "../ui/Icon";

/** First run: connect the cloud, then pick what to sync. Everything here is also reachable later. */
export function SetupScreen() {
  const storage = useAppState((s) => s.storage);
  const [step, setStep] = useState<0 | 1>(storage?.connected ? 1 : 0);
  return (
    <div className="wizard">
      <div className="brand" style={{ padding: 0 }}>
        <span className="brand-mark" aria-hidden="true">
          <Icon name="box" size={17} />
        </span>
        <span>PackSync</span>
      </div>
      <div className="wizard-steps" aria-label="Setup steps">
        <span className={step === 0 ? "on" : ""}>1 · Connect</span>
        <span aria-hidden="true">›</span>
        <span className={step === 1 ? "on" : ""}>2 · Pick instances</span>
      </div>
      {step === 0 ? (
        <>
          <div className="col" style={{ gap: 4 }}>
            <h1 className="hero-title">Your modpacks, on every computer</h1>
            <p className="muted">
              Stop playing on one computer and carry on from another: mods, settings and worlds come with you.
            </p>
          </div>
          <StorageSetup onConnected={() => setStep(1)} />
          <div className="row">
            <Button variant="ghost" onClick={() => navigate({ name: "instances" })}>
              Skip for now
            </Button>
          </div>
        </>
      ) : (
        <>
          <div className="col" style={{ gap: 4 }}>
            <h1>Choose what to sync</h1>
            <p className="muted">PackSync found these Prism instances. You see exactly what would be synced before anything starts.</p>
          </div>
          {storage?.connected ? <Banner tone="ok" title="Cloud connected">{storage.detail}</Banner> : null}
          <InstanceCardLite />
          <div className="row">
            <Button variant="primary" onClick={() => navigate({ name: "instances" })}>
              Finish
            </Button>
            <Button variant="ghost" onClick={() => navigate({ name: "cloud" })}>
              I already use PackSync on another computer
            </Button>
          </div>
        </>
      )}
    </div>
  );
}
