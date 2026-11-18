import type { ReactNode } from "react";
import { useStore } from "../state/context";
import { Button, Field } from "../ui/controls";

/** A file or folder path: a text field, plus a native "Browse…" button when the desktop shell can show one. */
export function PathField({
  label,
  value,
  onChange,
  hint,
  placeholder,
  error,
  kind = "folder",
  browseTitle,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  hint?: ReactNode;
  placeholder?: string;
  error?: string | null;
  kind?: "folder" | "file";
  browseTitle?: string;
}) {
  const store = useStore();
  const pick = kind === "file" ? store.transport.pickFile : store.transport.pickFolder;
  const canBrowse = typeof pick === "function" && store.transport.kind === "tauri";
  const title = browseTitle ?? (kind === "file" ? "Choose a file" : "Choose a folder");
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
                const p = await pick.call(store.transport, title);
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
