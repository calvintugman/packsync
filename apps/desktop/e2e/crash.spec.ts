import type { Page } from "@playwright/test";
import { NAME } from "./scenarios";
import { card, enableViaApi, expect, expectCardPhase, openApp, test, toast } from "./world";
import type { World } from "./world";

test.use({ connected: true });

const HALF = "saves/World 0/stats/half-written.json";

async function crashOnce(page: Page, world: World) {
  await enableViaApi(world, NAME);
  await world.control({ cmd: "fake_game", secs: 0.8, exit: 1, crash_report: true, write: [HALF, "{"] });
  await openApp(page, world);
  await card(page, NAME).getByRole("button", { name: "Play" }).click();
  await expect(toast(page, /did not end normally/)).toBeVisible();
  await expectCardPhase(page, NAME, "crash_pending");
}

test("after a crash the save is quarantined and Play waits for a decision", async ({ page, world }) => {
  await crashOnce(page, world);
  const c = card(page, NAME);
  await expect(c.getByText("The last session did not end normally", { exact: true })).toBeVisible();
  await expect(c.getByRole("button", { name: "Play" })).toBeDisabled();
  // nothing was uploaded as the new head: the cloud still has only the first snapshot
  const cloud = await world.control<{ heads: { id: string }[] }[]>({ cmd: "cloud" });
  expect(cloud[0]!.heads).toHaveLength(1);
});

test("crash: go back to the last good save", async ({ page, world }) => {
  await crashOnce(page, world);
  await card(page, NAME).getByRole("button", { name: "Review the crash save…" }).click();
  const dialog = page.getByRole("dialog", { name: `${NAME} did not close normally` });
  await expect(dialog.getByText("Crash save", { exact: true })).toBeVisible();
  await dialog.getByRole("radio", { name: /Go back to the last good save/ }).check();
  await dialog.getByRole("button", { name: "Apply this choice" }).click();
  await expect(toast(page, /Went back to the last good save/)).toBeVisible();
  await expect(dialog).toBeHidden();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
  expect(world.hasInstanceFile(NAME, HALF)).toBe(false);
});

test("crash: keep what is on disk and continue from it", async ({ page, world }) => {
  await crashOnce(page, world);
  await card(page, NAME).getByRole("button", { name: "Review the crash save…" }).click();
  const dialog = page.getByRole("dialog", { name: `${NAME} did not close normally` });
  await dialog.getByRole("radio", { name: /Keep it — continue from here/ }).check();
  await dialog.getByRole("button", { name: "Apply this choice" }).click();
  await expect(toast(page, /Kept\. This is now your latest progress/)).toBeVisible();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
  expect(world.hasInstanceFile(NAME, HALF)).toBe(true);
});
