import type { ReactNode } from "react";
import { describeError, isTransient } from "../api/errors";
import { sentence } from "../lib/format";
import { navigate } from "../lib/router";
import { Banner, Button } from "./controls";

/** An error with what happened, what to do about it, and — when there is one — the button for it. */
export function ErrorNote({
  error,
  onSignIn,
  onRetry,
  instanceId,
  actions: extra,
}: {
  error: unknown;
  onSignIn?: () => void;
  /** Offered for problems that may pass by themselves (no network, rate limit). */
  onRetry?: () => void;
  instanceId?: string;
  /** Anything else the situation calls for, such as an explicit "play offline" choice. */
  actions?: ReactNode;
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
      {extra}
      {onRetry && isTransient(error) ? (
        <Button size="sm" onClick={onRetry}>
          Try again
        </Button>
      ) : null}
    </>
  );
  const hasActions =
    Boolean(extra) ||
    Boolean(onRetry && isTransient(error)) ||
    (d.action === "sign_in" ? Boolean(onSignIn) : d.action !== undefined && (instanceId !== undefined || d.action === "open_settings"));
  return (
    <Banner tone="danger" title={d.title} actions={hasActions ? actions : undefined}>
      <span>{sentence(d.message)}</span>
      {d.advice ? <span className="muted"> {d.advice}</span> : null}
    </Banner>
  );
}
