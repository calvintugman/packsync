import { mkdirSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { REPO_ROOT } from "./paths";
import { NAME } from "./scenarios";
import { card, enableViaApi, expect, openApp, test } from "./world";
import type { World } from "./world";

// Pictures for the README, taken from the real UI running against the real engine in the test world.
// They are regenerated on demand and are not part of the normal run:
//   SCREENSHOTS=1 npx playwright test e2e/screenshots.spec.ts
test.skip(!process.env.SCREENSHOTS, "set SCREENSHOTS=1 to regenerate the README screenshot");

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
  test.use({ connected: true, hosted: true, viewport: { width: 1180, height: 760 } });

  test("instances", async ({ page, world }) => {
    await lived(world);
    await openApp(page, world);
    await expect(card(page, NAME)).toBeVisible();
    await card(page, NAME).getByRole("button", { name: "Sync now" }).click();
    await expect(page.getByRole("status").filter({ hasText: /Updated from the cloud/ })).toBeVisible();
    await page.getByRole("status").filter({ hasText: /Updated from the cloud/ }).getByRole("button", { name: "Dismiss" }).click();
    await expect(card(page, "Vanilla Plus")).toHaveAttribute("data-phase", "local_snapshot_complete");
    await shot(page, "instances");
  });
});
