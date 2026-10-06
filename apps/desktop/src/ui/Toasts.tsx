import { useEffect } from "react";
import { useAppState, useStore } from "../state/context";
import type { Toast } from "../state/store";
import { Button } from "./controls";

function ToastItem({ t, onClose }: { t: Toast; onClose: () => void }) {
  useEffect(() => {
    const id = setTimeout(onClose, t.level === "error" ? 12_000 : 6_000);
    return () => clearTimeout(id);
  }, [t.level, onClose]);
  return (
    <div className={`toast ${t.level}`} role={t.level === "error" ? "alert" : "status"}>
      <div className="col grow" style={{ gap: 2 }}>
        {t.title ? <strong>{t.title}</strong> : null}
        <span>{t.message}</span>
      </div>
      <Button variant="ghost" size="sm" icon="x" aria-label="Dismiss" onClick={onClose} />
    </div>
  );
}

export function Toasts() {
  const store = useStore();
  const toasts = useAppState((s) => s.toasts);
  if (toasts.length === 0) return null;
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((t) => (
        <ToastItem key={t.id} t={t} onClose={() => store.dismissToast(t.id)} />
      ))}
    </div>
  );
}
