import { useState } from "react";
import type { RuleDecision, Suggestion } from "../api/types";
import { formatBytes, plural } from "../lib/format";
import { Checkbox } from "../ui/controls";

/** The person's answers, by path; starts from PackSync's proposals. */
export type Choices = Record<string, boolean>;

export function proposals(s: Suggestion[]): Choices {
  return Object.fromEntries(s.map((x) => [x.path, x.include]));
}

export function asDecisions(c: Choices): RuleDecision[] {
  return Object.entries(c).map(([path, include]) => ({ path, include }));
}

function Item({ s, value, onChange }: { s: Suggestion; value: boolean; onChange: (v: boolean) => void }) {
  return (
    <Checkbox checked={value} onChange={onChange} hint={`${s.reason} · ${plural(s.files, "file")}, ${formatBytes(s.bytes)}`}>
      <code>{s.path}</code>
    </Checkbox>
  );
}

/**
 * Content no rule covers: what PackSync is unsure about first ("your call"), what it recognised folded
 * away but still changeable. Ticked means synced.
 */
export function UnknownChoices({ suggestions, value, onChange }: { suggestions: Suggestion[]; value: Choices; onChange: (c: Choices) => void }) {
  const [showSure, setShowSure] = useState(false);
  const unsure = suggestions.filter((s) => !s.sure);
  const sure = suggestions.filter((s) => s.sure);
  const set = (path: string) => (v: boolean) => onChange({ ...value, [path]: v });
  if (suggestions.length === 0) return null;
  return (
    <section className="card col" aria-label="Files PackSync does not recognise">
      <div className="col" style={{ gap: 2 }}>
        <h3>Mod data PackSync found</h3>
        <p className="muted">
          These are not covered by PackSync's rules. Ticked ones are synced to your other computers. Your choice is
          remembered, and new ones are asked about once.
        </p>
      </div>
      {unsure.length > 0 ? (
        <div className="col" role="group" aria-label="Your call">
          <strong>Your call</strong>
          {unsure.map((s) => (
            <Item key={s.path} s={s} value={value[s.path] ?? s.include} onChange={set(s.path)} />
          ))}
        </div>
      ) : null}
      {sure.length > 0 ? (
        <div className="col">
          <button type="button" className="link-btn" aria-expanded={showSure} onClick={() => setShowSure((v) => !v)}>
            {showSure ? "Hide" : "Show"} {plural(sure.length, "recognised item")} (
            {sure.filter((s) => value[s.path] ?? s.include).length} synced)
          </button>
          {showSure ? (
            <div className="col" role="group" aria-label="Recognised automatically">
              {sure.map((s) => (
                <Item key={s.path} s={s} value={value[s.path] ?? s.include} onChange={set(s.path)} />
              ))}
            </div>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}
