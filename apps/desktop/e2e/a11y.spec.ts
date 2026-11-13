import AxeBuilder from "@axe-core/playwright";
import type { Page } from "@playwright/test";
import { makeConflict, NAME } from "./scenarios";
import { card, enableViaApi, expect, openApp, test } from "./world";

// check the settled screen: entry animations fade elements in, and mid-fade text is measured as faint
test.use({ reducedMotion: "reduce" });

test.use({ connected: true });

async function expectAccessible(page: Page, what: string) {
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  const report = results.violations.map((v) => `${v.id} (${v.impact}): ${v.help}\n  ${v.nodes.map((n) => n.target.join(" ")).join("\n  ")}`);
  expect(report, `${what} has accessibility violations`).toEqual([]);
}

test("the main screens have no detectable accessibility violations", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await openApp(page, world);
  await expect(card(page, NAME)).toBeVisible();
  await expectAccessible(page, "instances");

  for (const hash of [`#/instance/${id}/history`, `#/instance/${id}/rules`, `#/instance/${id}/hooks`, `#/instance/${id}/recovery`, "#/cloud", "#/activity", "#/settings"]) {
    await page.evaluate((h) => (window.location.hash = h), hash);
    await expect(page.getByRole("main")).toBeVisible();
    await page.waitForTimeout(300);
    await expectAccessible(page, hash);
  }
});

test("dialogs are accessible too", async ({ page, world }) => {
  await openApp(page, world);
  await page.getByRole("listitem").filter({ hasText: "Skyblock Pack" }).getByRole("button", { name: /Start syncing/ }).click();
  const dialog = page.getByRole("dialog", { name: "Start syncing this instance" });
  await expect(dialog.getByText("files will be synced")).toBeVisible();
  await expectAccessible(page, "enable dialog");
});

test("the conflict dialog is accessible", async ({ page, world }) => {
  await makeConflict(world);
  await openApp(page, world);
  await card(page, NAME).getByRole("button", { name: "Sync now" }).click();
  await expect(page.getByRole("dialog", { name: `Two versions of ${NAME}` })).toBeVisible();
  await expectAccessible(page, "conflict dialog");
});

test.describe("dark mode", () => {
  test.use({ colorScheme: "dark" });
  test("is accessible in the dark colour scheme", async ({ page, world }) => {
    await enableViaApi(world, NAME);
    await openApp(page, world);
    await expect(card(page, NAME)).toBeVisible();
    await expectAccessible(page, "instances (dark)");
    await page.evaluate(() => (window.location.hash = "#/settings"));
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
    await expectAccessible(page, "settings (dark)");
  });
});
