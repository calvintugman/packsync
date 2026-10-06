import type { SyncPhase } from "../api/types";
import { PHASES, STEPS } from "../lib/phases";

/** The four stages between "saved on this computer" and "ready on your other computers". */
export function Stepper({ phase }: { phase: SyncPhase }) {
  const cur = PHASES[phase].step;
  if (cur === null) return null;
  return (
    <ol className="stepper" aria-label="Upload progress">
      {STEPS.map((s, i) => {
        const cls = i < cur ? "done" : i === cur ? `current ${i === 3 ? "final" : ""}` : "";
        return (
          <li key={s.label} className={cls} aria-current={i === cur ? "step" : undefined} title={s.hint}>
            <span className="bar" />
            <span>{s.label}</span>
          </li>
        );
      })}
    </ol>
  );
}
