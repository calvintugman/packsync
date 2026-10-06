import { useId } from "react";
import type { ButtonHTMLAttributes, InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from "react";
import type { Tone } from "../lib/phases";
import { clamp01 } from "../lib/format";
import { Icon } from "./Icon";
import type { IconName } from "./Icon";

export function Spinner({ label }: { label?: string }) {
  return <span className="spinner" role={label ? "status" : undefined} aria-label={label} aria-hidden={label ? undefined : true} />;
}

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: "primary" | "secondary" | "danger" | "ghost";
  size?: "sm" | "md";
  busy?: boolean;
  icon?: IconName;
}

export function Button({ variant = "secondary", size = "md", busy, icon, children, className, disabled, type, ...rest }: ButtonProps) {
  const cls = ["btn", variant === "secondary" ? "" : variant, size === "sm" ? "sm" : "", className ?? ""].filter(Boolean).join(" ");
  return (
    <button type={type ?? "button"} className={cls} disabled={disabled || busy} aria-busy={busy || undefined} {...rest}>
      {busy ? <Spinner /> : icon ? <Icon name={icon} size={size === "sm" ? 15 : 17} /> : null}
      {children}
    </button>
  );
}

export function Pill({ tone = "neutral", children, title }: { tone?: Tone; children: ReactNode; title?: string }) {
  return (
    <span className={`pill ${tone === "neutral" ? "" : tone}`} title={title}>
      {children}
    </span>
  );
}

export function Tag({ children, title }: { children: ReactNode; title?: string }) {
  return (
    <span className="tag" title={title}>
      {children}
    </span>
  );
}

export function ProgressBar({ value, label }: { value?: number | null; label: string }) {
  const known = typeof value === "number" && Number.isFinite(value);
  return (
    <div
      className={`progress ${known ? "" : "indeterminate"}`}
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={known ? Math.round(clamp01(value) * 100) : undefined}
    >
      <span style={known ? { width: `${clamp01(value) * 100}%` } : undefined} />
    </div>
  );
}

export function Banner({
  tone = "info",
  title,
  children,
  actions,
  role,
}: {
  tone?: "info" | "warn" | "danger" | "ok";
  title?: ReactNode;
  children?: ReactNode;
  actions?: ReactNode;
  role?: "alert" | "status";
}) {
  const icon: IconName = tone === "ok" ? "check" : tone === "info" ? "info" : "alert";
  return (
    <div className={`banner ${tone}`} role={role ?? (tone === "danger" ? "alert" : undefined)}>
      <Icon name={icon} />
      <div className="banner-body">
        {title ? <div className="banner-title">{title}</div> : null}
        {children ? <div>{children}</div> : null}
        {actions ? <div className="banner-actions">{actions}</div> : null}
      </div>
    </div>
  );
}

export function EmptyState({ title, children, action }: { title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty">
      <h2>{title}</h2>
      {children ? <p>{children}</p> : null}
      {action}
    </div>
  );
}

interface FieldProps {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  children: (id: string, describedBy: string | undefined) => ReactNode;
}

export function Field({ label, hint, error, children }: FieldProps) {
  const id = useId();
  const hintId = hint ? `${id}-hint` : undefined;
  const errId = error ? `${id}-err` : undefined;
  const describedBy = [hintId, errId].filter(Boolean).join(" ") || undefined;
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      {children(id, describedBy)}
      {hint ? (
        <span className="hint" id={hintId}>
          {hint}
        </span>
      ) : null}
      {error ? (
        <span className="error-text" id={errId} role="alert">
          {error}
        </span>
      ) : null}
    </div>
  );
}

export function TextInput({ label, hint, error, ...rest }: { label: string; hint?: ReactNode; error?: string | null } & InputHTMLAttributes<HTMLInputElement>) {
  return (
    <Field label={label} hint={hint} error={error}>
      {(id, d) => <input id={id} className="input" aria-describedby={d} aria-invalid={error ? true : undefined} {...rest} />}
    </Field>
  );
}

export function TextArea({ label, hint, ...rest }: { label: string; hint?: ReactNode } & TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <Field label={label} hint={hint}>{(id, d) => <textarea id={id} className="textarea" aria-describedby={d} {...rest} />}</Field>;
}

export function Select({ label, hint, children, ...rest }: { label: string; hint?: ReactNode } & SelectHTMLAttributes<HTMLSelectElement>) {
  return <Field label={label} hint={hint}>{(id, d) => <select id={id} className="select" aria-describedby={d} {...rest}>{children}</select>}</Field>;
}

export function Checkbox({
  checked,
  onChange,
  children,
  disabled,
  hint,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  children: ReactNode;
  disabled?: boolean;
  hint?: ReactNode;
}) {
  const id = useId();
  return (
    <div className="check">
      <input id={id} type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <div className="col" style={{ gap: 2 }}>
        <label htmlFor={id}>{children}</label>
        {hint ? <span className="hint muted">{hint}</span> : null}
      </div>
    </div>
  );
}

export function Choice({
  name,
  value,
  selected,
  onSelect,
  title,
  children,
  disabled,
}: {
  name: string;
  value: string;
  selected: boolean;
  onSelect: (v: string) => void;
  title: ReactNode;
  children?: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className={`choice ${selected ? "selected" : ""}`}>
      <input type="radio" name={name} value={value} checked={selected} disabled={disabled} onChange={() => onSelect(value)} />
      <span className="col" style={{ gap: 2 }}>
        <span className="title">{title}</span>
        {children ? <span className="muted">{children}</span> : null}
      </span>
    </label>
  );
}

export function Kv({ rows }: { rows: [ReactNode, ReactNode][] }) {
  return (
    <dl className="kv">
      {rows.map(([k, v], i) => (
        <div key={i} style={{ display: "contents" }}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function StatusDot({ tone }: { tone: "ok" | "warn" | "danger" | "neutral" }) {
  return <span className={`status-dot ${tone === "neutral" ? "" : tone}`} aria-hidden="true" />;
}
