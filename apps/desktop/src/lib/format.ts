import type { PackIdentity, WorldInfo } from "../api/types";

const UNITS = ["B", "KB", "MB", "GB", "TB"] as const;

/** Decimal units, like the command line and Google Drive's own quota display. */
export function formatBytes(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n) || n < 0) return "–";
  let v = n;
  let u = 0;
  while (v >= 1000 && u < UNITS.length - 1) {
    v /= 1000;
    u += 1;
  }
  if (u === 0) return `${Math.round(v)} B`;
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${UNITS[u]}`;
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n.toLocaleString("en")} ${n === 1 ? one : many}`;
}

function toMs(t: string | number | null | undefined): number | null {
  if (t === null || t === undefined) return null;
  if (typeof t === "number") return t;
  const ms = Date.parse(t);
  return Number.isNaN(ms) ? null : ms;
}

const REL = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

/** "3 minutes ago", "yesterday", "in 2 hours". Falls back to the date after a month. */
export function formatRelative(t: string | number | null | undefined, now: number = Date.now()): string {
  const ms = toMs(t);
  if (ms === null) return "never";
  const diff = ms - now;
  const abs = Math.abs(diff);
  const sec = abs / 1000;
  if (sec < 45) return diff > 0 ? "in a few seconds" : "just now";
  const table: [number, Intl.RelativeTimeFormatUnit, number][] = [
    [60 * 60, "minute", 60],
    [60 * 60 * 24, "hour", 3600],
    [60 * 60 * 24 * 30, "day", 86400],
  ];
  for (const [limit, unit, per] of table) {
    if (sec < limit) return REL.format(Math.round(diff / 1000 / per), unit);
  }
  return formatDate(ms);
}

export function formatDate(t: string | number | null | undefined): string {
  const ms = toMs(t);
  if (ms === null) return "–";
  return new Date(ms).toLocaleDateString("en", { year: "numeric", month: "short", day: "numeric" });
}

export function formatDateTime(t: string | number | null | undefined): string {
  const ms = toMs(t);
  if (ms === null) return "–";
  return new Date(ms).toLocaleString("en", {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${s % 60} s`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}

const LOADERS: Record<string, string> = {
  forge: "Forge",
  neoforge: "NeoForge",
  fabric: "Fabric",
  quilt: "Quilt",
  liteloader: "LiteLoader",
};

/** "Minecraft 1.20.1 · Forge 47.4.0" */
export function packLine(pack: PackIdentity): string {
  const parts: string[] = [];
  if (pack.minecraft_version) parts.push(`Minecraft ${pack.minecraft_version}`);
  if (pack.loader) parts.push(`${LOADERS[pack.loader.kind] ?? pack.loader.kind} ${pack.loader.version}`);
  return parts.join(" · ") || "Unknown version";
}

export function worldName(w: WorldInfo): string {
  return w.name?.trim() || w.folder;
}

export function worldsLine(worlds: WorldInfo[]): string {
  if (worlds.length === 0) return "No worlds yet";
  if (worlds.length === 1) return `World “${worldName(worlds[0]!)}”`;
  return plural(worlds.length, "world");
}

/** The last path component, for either separator. */
export function baseName(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export function shortId(id: string): string {
  return id.slice(0, 8);
}

/** Make a message read as a sentence: engine messages are fragments, and are followed by advice. */
export function sentence(s: string): string {
  const t = s.trim();
  return /[.!?…]$/.test(t) ? t : `${t}.`;
}

export function clamp01(x: number): number {
  return Math.min(1, Math.max(0, x));
}
