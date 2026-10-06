import { spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test as base } from "@playwright/test";
import type { Page } from "@playwright/test";
import { CLI_BIN, DIST, E2E_BIN } from "./paths";

interface Started {
  port: number;
  token: string;
  url: string;
  control: string;
  cloud: string;
  dir: string;
  desktop_prism: string;
}

/** What a test can do besides clicking: be "the other computer", edit files, change what the game does. */
export class World {
  constructor(
    private readonly info: Started,
    private readonly child: ChildProcess,
  ) {}

  get url(): string {
    return this.info.url;
  }
  get port(): number {
    return this.info.port;
  }
  get token(): string {
    return this.info.token;
  }
  get cloud(): string {
    return this.info.cloud;
  }
  get dir(): string {
    return this.info.dir;
  }
  get desktopPrism(): string {
    return this.info.desktop_prism;
  }

  async control<T = unknown>(cmd: Record<string, unknown>): Promise<T> {
    const res = await fetch(this.info.control, { method: "POST", body: JSON.stringify(cmd) });
    const doc = (await res.json()) as { ok: boolean; result?: T; error?: string };
    if (!doc.ok) throw new Error(`control ${String(cmd.cmd)} failed: ${doc.error}`);
    return doc.result as T;
  }

  /** Call the desktop's application layer directly (what the UI does through the bridge). */
  async api<T = unknown>(method: string, params?: unknown): Promise<T> {
    const res = await fetch(`http://127.0.0.1:${this.info.port}/api/call`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-packsync-token": this.info.token },
      body: JSON.stringify({ method, params: params ?? null }),
    });
    const doc = (await res.json()) as { ok: boolean; result?: T; error?: unknown };
    if (!doc.ok) throw new Error(`${method} failed: ${JSON.stringify(doc.error)}`);
    return doc.result as T;
  }

  instanceRoot(name: string): string {
    return join(this.info.desktop_prism, "instances", name);
  }

  readInstanceFile(name: string, rel: string): string {
    return readFileSync(join(this.instanceRoot(name), ".minecraft", rel), "utf8");
  }

  hasInstanceFile(name: string, rel: string): boolean {
    return existsSync(join(this.instanceRoot(name), ".minecraft", rel));
  }

  async stop(): Promise<void> {
    this.child.kill("SIGKILL");
    await new Promise((r) => this.child.once("exit", r));
    rmSync(this.info.dir, { recursive: true, force: true });
  }
}

async function startWorld(connected: boolean): Promise<World> {
  const dir = mkdtempSync(join(tmpdir(), "packsync-e2e-"));
  const args = ["--dir", dir, "--ui-dir", DIST, "--cli", CLI_BIN, ...(connected ? ["--connected"] : [])];
  const child = spawn(E2E_BIN, args, {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, PACKSYNC_LAUNCH_POLL_MS: "100" },
  });
  let stderr = "";
  child.stderr.on("data", (d: Buffer) => (stderr += d.toString()));
  const info = await new Promise<Started>((resolve, reject) => {
    let buf = "";
    child.stdout.on("data", (d: Buffer) => {
      buf += d.toString();
      const nl = buf.indexOf("\n");
      if (nl >= 0) {
        try {
          resolve(JSON.parse(buf.slice(0, nl)) as Started);
        } catch (e) {
          reject(e);
        }
      }
    });
    child.once("exit", (c) => reject(new Error(`the test world exited with ${c}: ${stderr}`)));
    setTimeout(() => reject(new Error(`the test world did not start: ${stderr}`)), 30_000);
  });
  return new World(info, child);
}

interface Fixtures {
  /** A fresh world per test. */
  world: World;
  /** Start with the desktop already connected to the shared cloud folder. */
  connected: boolean;
}

export const test = base.extend<Fixtures>({
  connected: [false, { option: true }],
  world: async ({ connected }, use) => {
    const w = await startWorld(connected);
    try {
      await use(w);
    } finally {
      await w.stop();
    }
  },
});

export { expect };

/** Open the app the way a person would: the printed address, token included. */
export async function openApp(page: Page, world: World, hash = ""): Promise<void> {
  await page.goto(world.url);
  await expect(page.getByRole("navigation", { name: "Main" }).or(page.getByRole("heading", { name: /Continue your modpack/ }))).toBeVisible();
  if (hash) await page.evaluate((h) => (window.location.hash = h), hash);
}

export function card(page: Page, name: string) {
  return page.getByRole("article", { name });
}

/** Start syncing one of the discovered instances through the dialog, as a person would. */
export async function enableInstance(page: Page, name: string): Promise<void> {
  await page.evaluate(() => (window.location.hash = "#/"));
  const row = page.getByRole("listitem").filter({ hasText: name });
  await row.getByRole("button", { name: /Start syncing/ }).click();
  const dialog = page.getByRole("dialog", { name: "Start syncing this instance" });
  await expect(dialog.getByText("files will be synced")).toBeVisible();
  await dialog.getByRole("button", { name: "Start syncing" }).click();
  await expect(page.getByRole("heading", { level: 1, name })).toBeVisible();
}

/** Start syncing a Prism instance of the desktop through the application layer (setup, not the thing under test). */
export async function enableViaApi(world: World, name: string): Promise<string> {
  const r = await world.api<{ outcome: { instance: string } }>("instances.enable", {
    path: world.instanceRoot(name),
    launcher: "prism",
    upload: true,
  });
  return r.outcome.instance;
}

export function toast(page: Page, text: string | RegExp) {
  return page.getByRole("status").filter({ hasText: text }).or(page.getByRole("alert").filter({ hasText: text })).first();
}

/** The sync phase the UI shows for an instance card — read from the DOM, not from the wording. */
export async function expectCardPhase(page: Page, name: string, phase: string): Promise<void> {
  await expect(card(page, name)).toHaveAttribute("data-phase", phase);
}

/** The same on the instance's detail page. */
export async function expectDetailPhase(page: Page, phase: string): Promise<void> {
  await expect(page.locator(".page-head[data-phase]")).toHaveAttribute("data-phase", phase);
}
