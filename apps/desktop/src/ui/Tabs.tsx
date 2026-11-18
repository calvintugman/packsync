import type { KeyboardEvent, ReactNode } from "react";

export interface TabDef {
  id: string;
  label: string;
}

export function Tabs({ tabs, active, onChange, children, idPrefix = "tab" }: { tabs: TabDef[]; active: string; onChange: (id: string) => void; children: ReactNode; idPrefix?: string }) {
  function onKey(e: KeyboardEvent<HTMLDivElement>) {
    const i = tabs.findIndex((t) => t.id === active);
    if (i < 0 || tabs.length === 0) return;
    let nextIdx: number | null = null;
    if (e.key === "ArrowRight") nextIdx = (i + 1) % tabs.length;
    else if (e.key === "ArrowLeft") nextIdx = (i - 1 + tabs.length) % tabs.length;
    else if (e.key === "Home") nextIdx = 0;
    else if (e.key === "End") nextIdx = tabs.length - 1;
    if (nextIdx === null) return;
    e.preventDefault();
    onChange(tabs[nextIdx]!.id);
    e.currentTarget.querySelectorAll<HTMLElement>('[role="tab"]')[nextIdx]?.focus();
  }
  return (
    <div className="col" style={{ gap: 14 }}>
      <div className="tabs" role="tablist" onKeyDown={onKey}>
        {tabs.map((t) => (
          <button
            key={t.id}
            id={`${idPrefix}-${t.id}`}
            role="tab"
            type="button"
            className="tab"
            aria-selected={t.id === active}
            aria-controls={`${idPrefix}-panel`}
            tabIndex={t.id === active ? 0 : -1}
            onClick={() => onChange(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div id={`${idPrefix}-panel`} role="tabpanel" aria-labelledby={`${idPrefix}-${active}`} className="col" style={{ gap: 14 }}>
        {children}
      </div>
    </div>
  );
}
