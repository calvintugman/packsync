import { makeConflict, NAME } from "./scenarios";
import { card, expect, expectCardPhase, openApp, test, toast } from "./world";
import type { Page } from "@playwright/test";

test.use({ connected: true });

const DESKTOP = "saves/World 0/stats/desktop.json";
const LAPTOP = "saves/World 0/stats/laptop.json";

async function openConflict(page: Page) {
  await card(page, NAME).getByRole("button", { name: "Sync now" }).click();
  const dialog = page.getByRole("dialog", { name: `Two versions of ${NAME}` });
  await expect(dialog).toBeVisible();
  return dialog;
}

test("a conflict shows both versions and never merges worlds", async ({ page, world }) => {
  await makeConflict(world);
  await openApp(page, world);
  await expectCardPhase(page, NAME, "ready_on_other_devices"); // the desktop does not know yet
  const dialog = await openConflict(page);

  await expect(dialog.getByText("PackSync never merges worlds automatically")).toBeVisible();
  await expect(dialog.getByRole("region", { name: "This computer" })).toContainText("Desktop");
  await expect(dialog.getByRole("region", { name: "In the cloud" })).toContainText("Laptop");
  await expect(dialog.getByText("Nothing is deleted:")).toBeVisible();

  // "Decide later" leaves everything as it is, and the card keeps asking
  await dialog.getByRole("button", { name: "Decide later" }).click();
  await expectCardPhase(page, NAME, "conflict");
  await expect(card(page, NAME).getByRole("button", { name: "Play" })).toBeDisabled();
  await expect(card(page, NAME).getByText("Two versions need your decision")).toBeVisible();
  expect(world.hasInstanceFile(NAME, DESKTOP)).toBe(true);
  expect(world.hasInstanceFile(NAME, LAPTOP)).toBe(false);
});

test("keep this computer's version", async ({ page, world }) => {
  await makeConflict(world);
  await openApp(page, world);
  const dialog = await openConflict(page);
  await dialog.getByRole("radio", { name: /Keep this computer's version/ }).check();
  await dialog.getByRole("button", { name: "Apply this choice" }).click();
  await expect(toast(page, "Conflict resolved. The other version stays in History.")).toBeVisible();
  await expect(dialog).toBeHidden();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
  expect(world.hasInstanceFile(NAME, DESKTOP)).toBe(true);
  expect(world.hasInstanceFile(NAME, LAPTOP)).toBe(false);
  const cloud = await world.control<{ name: string; heads: unknown[] }[]>({ cmd: "cloud" });
  expect(cloud).toHaveLength(1);
  expect(cloud[0]!.heads).toHaveLength(1);
});

test("use the cloud's version: this computer's changes stay in the history", async ({ page, world }) => {
  const id = await makeConflict(world);
  await openApp(page, world);
  const dialog = await openConflict(page);
  await dialog.getByRole("radio", { name: /Use the cloud's version/ }).check();
  await dialog.getByRole("button", { name: "Apply this choice" }).click();
  await expect(toast(page, "Conflict resolved. The other version stays in History.")).toBeVisible();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
  expect(world.hasInstanceFile(NAME, LAPTOP)).toBe(true);
  expect(world.hasInstanceFile(NAME, DESKTOP)).toBe(false);
  // the desktop's version was kept as a snapshot before it was replaced
  const history = await world.api<{ device: string | null }[]>("instance.history", { instance: id, online: false });
  expect(history.some((h) => h.device === "Desktop")).toBe(true);
});

test("keep both: this computer's version becomes its own instance", async ({ page, world }) => {
  await makeConflict(world);
  await openApp(page, world);
  const dialog = await openConflict(page);
  await dialog.getByRole("radio", { name: /Keep both, as separate instances/ }).check();
  await dialog.getByLabel("Name of the separate instance").fill("Skyblock Experiments");
  await dialog.getByRole("button", { name: "Apply this choice" }).click();
  await expect(toast(page, /continues as “Skyblock Experiments”/)).toBeVisible();
  await expect(card(page, "Skyblock Experiments")).toBeVisible();
  await expectCardPhase(page, "Skyblock Experiments", "ready_on_other_devices");
  expect(world.hasInstanceFile(NAME, DESKTOP)).toBe(true);
  const cloud = await world.control<{ name: string }[]>({ cmd: "cloud" });
  expect(cloud.map((c) => c.name).sort()).toEqual(["Skyblock Experiments", NAME]);
});
