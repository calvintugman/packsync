import type { ReactNode } from "react";
import { useStore } from "../state/context";
import { Button, Field } from "../ui/controls";

/** A folder path: a text field, plus a native "Browse…" button when the desktop shell can show one. */
export function PathField({
  label,
  value,
  onChange,
  hint,
  placeholder,
  error,
  browseTitle = "Choose a folder",
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  hint?: ReactNode;
  placeholder?: string;
  error?: string | null;
  browseTitle?: string;
}) {
  const store = useStore();
  const canBrowse = typeof store.transport.pickFolder === "function" && store.transport.kind === "tauri";
  return (
    <Field label={label} hint={hint} error={error}>
      {(id, d) => (
        <div className="row">
          <input
            id={id}
            className="input"
            value={value}
            placeholder={placeholder}
            spellCheck={false}
            aria-describedby={d}
            aria-invalid={error ? true : undefined}
            onChange={(e) => onChange(e.target.value)}
          />
          {canBrowse ? (
            <Button
              icon="folder"
              onClick={async () => {
                const p = await store.transport.pickFolder?.(browseTitle);
                if (p) onChange(p);
              }}
            >
              Browse…
            </Button>
          ) : null}
        </div>
      )}
    </Field>
  );
}
