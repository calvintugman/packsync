import { describeError } from "../api/errors";
import { navigate } from "../lib/router";
import { Banner, Button } from "./controls";

/** An error with what happened, what to do about it, and — when there is one — the button for it. */
export function ErrorNote({
  error,
  onSignIn,
  instanceId,
}: {
  error: unknown;
  onSignIn?: () => void;
  instanceId?: string;
}) {
  if (!error) return null;
  const d = describeError(error);
  const actions = (
    <>
      {d.action === "sign_in" && onSignIn ? (
        <Button size="sm" variant="primary" onClick={onSignIn}>
          Sign in again
        </Button>
      ) : null}
      {d.action === "open_conflict" && instanceId ? (
        <Button size="sm" onClick={() => navigate({ name: "instance", id: instanceId, tab: null })}>
          Open the instance
        </Button>
      ) : null}
      {d.action === "open_crash" && instanceId ? (
        <Button size="sm" onClick={() => navigate({ name: "instance", id: instanceId, tab: null })}>
          Open the instance
        </Button>
      ) : null}
      {d.action === "open_settings" ? (
        <Button size="sm" onClick={() => navigate({ name: "settings" })}>
          Open settings
        </Button>
      ) : null}
    </>
  );
  const hasActions = d.action === "sign_in" ? Boolean(onSignIn) : d.action !== undefined && (instanceId !== undefined || d.action === "open_settings");
  return (
    <Banner tone="danger" title={d.title} actions={hasActions ? actions : undefined}>
      <span>{d.message}</span>
      {d.advice ? <span className="muted"> {d.advice}</span> : null}
    </Banner>
  );
}
