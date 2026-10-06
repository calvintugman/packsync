import type { ReactNode } from "react";
import { useAction } from "../state/context";
import { Button } from "./controls";
import { Dialog } from "./Dialog";
import { ErrorNote } from "./ErrorNote";

/** Ask before something that is hard to undo. The action's error is shown inside the dialog. */
export function ConfirmDialog({
  title,
  children,
  confirmLabel,
  danger,
  onConfirm,
  onClose,
}: {
  title: string;
  children: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => Promise<unknown>;
  onClose: () => void;
}) {
  const act = useAction(async () => {
    await onConfirm();
    onClose();
  });
  return (
    <Dialog
      title={title}
      onClose={onClose}
      locked={act.busy}
      footer={
        <>
          <Button onClick={onClose} disabled={act.busy} data-autofocus>
            Cancel
          </Button>
          <Button variant={danger ? "danger" : "primary"} busy={act.busy} onClick={() => void act.run()}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      {children}
      {act.error ? <ErrorNote error={act.error} /> : null}
    </Dialog>
  );
}
