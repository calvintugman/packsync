import { mkdirSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { REPO_ROOT } from "./paths";
import { makeConflict, NAME } from "./scenarios";
import { card, enableViaApi, expect, openApp, test } from "./world";
import type { World } from "./world";

// Pictures for the README, taken from the real UI running against the real engine in the test world.
// They are regenerated on demand and are not part of the normal run:
//   SCREENSHOTS=1 npx playwright test e2e/screenshots.spec.ts
test.skip(!process.env.SCREENSHOTS, "set SCREENSHOTS=1 to regenerate docs/images");

const OUT = join(REPO_ROOT, "docs", "images");
mkdirSync(OUT, { recursive: true });

async function shot(page: Page, name: string) {
  await page.waitForTimeout(250); // let transitions settle
  await page.screenshot({ path: join(OUT, `${name}.png`) });
}

/** A world with some history from "the laptop" so the screens are not empty. */
async function lived(world: World) {
  await enableViaApi(world, NAME);
  await world.control({ cmd: "laptop_restore", instance: NAME });
  await world.control({ cmd: "edit", device: "laptop", instance: NAME, path: "saves/World 0/stats/evening.json", content: "{}" });
  await world.control({ cmd: "sync", device: "laptop", instance: NAME });
  await world.api("instances.enable", { path: world.instanceRoot("Vanilla Plus"), launcher: "prism", upload: false });
}

test.describe("light", () => {
  test.use({ connected: true, viewport: { width: 1180, height: 760 } });

  test("instances", async ({ page, world }) => {
    await lived(world);
    await openApp(page, world);
    await expect(card(page, NAME)).toBeVisible();
    await card(page, NAME).getByRole("button", { name: "Sync now" }).click();
    await expect(page.getByRole("status").filter({ hasText: /Applied newer progress/ })).toBeVisible();
    await page.getByRole("status").filter({ hasText: /Applied newer progress/ }).getByRole("button", { name: "Dismiss" }).click();
    await expect(card(page, "Vanilla Plus")).toHaveAttribute("data-phase", "local_snapshot_complete");
    await shot(page, "instances");
  });

  test("instance history", async ({ page, world }) => {
    await lived(world);
    const list = await world.api<{ id: string; name: string }[]>("instances.list", { online: false });
    const id = list.find((i) => i.name === NAME)!.id;
    await world.api("instance.sync", { instance: id });
    await openApp(page, world, `#/instance/${id}/history`);
    await expect(page.getByRole("list", { name: "Snapshots" })).toBeVisible();
    await shot(page, "history");
  });

  test("conflict", async ({ page, world }) => {
    await makeConflict(world);
    await openApp(page, world);
    await card(page, NAME).getByRole("button", { name: "Sync now" }).click();
    await expect(page.getByRole("dialog", { name: `Two versions of ${NAME}` })).toBeVisible();
    await shot(page, "conflict");
  });

  test("preview before syncing", async ({ page, world }) => {
    await openApp(page, world);
    await page.getByRole("listitem").filter({ hasText: NAME }).getByRole("button", { name: /Start syncing/ }).click();
    await expect(page.getByRole("dialog").getByText("files will be synced")).toBeVisible();
    await shot(page, "preview");
  });

  test("cloud", async ({ page, world }) => {
    await world.control({ cmd: "laptop_new_instance", instance: "Laptop Pack" });
    await enableViaApi(world, NAME);
    await openApp(page, world, "#/cloud");
    await expect(page.getByRole("article", { name: "Laptop Pack" })).toBeVisible();
    await shot(page, "cloud");
  });

});

test.describe("first run", () => {
  test.use({ connected: false, viewport: { width: 1180, height: 820 } });
  test("setup", async ({ page, world }) => {
    await openApp(page, world);
    await expect(page.getByRole("heading", { name: "Continue your modpack on any computer" })).toBeVisible();
    await shot(page, "setup");
  });
});

test.describe("dark", () => {
  test.use({ connected: true, colorScheme: "dark", viewport: { width: 1180, height: 760 } });
  test("instances (dark)", async ({ page, world }) => {
    await lived(world);
    await openApp(page, world);
    await expect(card(page, "Vanilla Plus")).toHaveAttribute("data-phase", "local_snapshot_complete");
    await shot(page, "instances-dark");
  });
});
