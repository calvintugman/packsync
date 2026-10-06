import { NAME } from "./scenarios";
import { enableViaApi, expect, openApp, test, toast } from "./world";

test.use({ connected: true });

const EXTRA = "config/added-later.toml";

test("history: restore an older snapshot, see exactly what changes, keep a recovery point", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await world.control({ cmd: "edit", device: "desktop", instance: NAME, path: EXTRA, content: "later = true\n" });
  await world.control({ cmd: "sync", device: "desktop", instance: NAME });
  expect(world.hasInstanceFile(NAME, EXTRA)).toBe(true);

  await openApp(page, world, `#/instance/${id}/history`);
  const snaps = page.getByRole("list", { name: "Snapshots" }).getByRole("listitem");
  await expect(snaps).toHaveCount(2);
  // the newest is where this computer is: it can't be "restored"
  await expect(snaps.first().getByRole("button", { name: "Restore…" })).toBeDisabled();
  await snaps.last().getByRole("button", { name: "Restore…" }).click();

  const dialog = page.getByRole("dialog", { name: "Restore this version" });
  await expect(dialog.locator(".stat", { hasText: "files will be removed" })).toContainText("1");
  await expect(dialog.getByText("Examples of what changes")).toBeVisible();
  await dialog.getByRole("button", { name: "Restore", exact: true }).click();
  await expect(dialog.getByText("Restored", { exact: true })).toBeVisible();
  await expect(toast(page, "Restored.")).toBeVisible();
  await dialog.getByRole("button", { name: "Done" }).click();
  expect(world.hasInstanceFile(NAME, EXTRA)).toBe(false);

  // what the restore replaced is kept
  await page.getByRole("tab", { name: "Recovery points" }).click();
  await expect(page.getByRole("list", { name: "Recovery points" }).getByRole("listitem")).toHaveCount(1);
});

test("restoring is previewed and can be cancelled without touching anything", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await world.control({ cmd: "edit", device: "desktop", instance: NAME, path: EXTRA, content: "later = true\n" });
  await world.control({ cmd: "sync", device: "desktop", instance: NAME });
  await openApp(page, world, `#/instance/${id}/history`);
  await page.getByRole("list", { name: "Snapshots" }).getByRole("listitem").last().getByRole("button", { name: "Restore…" }).click();
  const dialog = page.getByRole("dialog", { name: "Restore this version" });
  await expect(dialog.getByText("files will be written")).toBeVisible();
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  expect(world.hasInstanceFile(NAME, EXTRA)).toBe(true);
});

test("unsaved changes are saved first when restoring", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await world.control({ cmd: "edit", device: "desktop", instance: NAME, path: EXTRA, content: "later = true\n" });
  await world.control({ cmd: "sync", device: "desktop", instance: NAME });
  // a change that has not been snapshotted yet
  await world.control({ cmd: "edit", device: "desktop", instance: NAME, path: "config/unsaved.toml", content: "x = 1\n" });
  await openApp(page, world, `#/instance/${id}/history`);
  await page.getByRole("list", { name: "Snapshots" }).getByRole("listitem").last().getByRole("button", { name: "Restore…" }).click();
  const dialog = page.getByRole("dialog", { name: "Restore this version" });
  await expect(dialog.getByText("You have changes that are not saved yet")).toBeVisible();
  await dialog.getByRole("button", { name: "Restore", exact: true }).click();
  await expect(dialog.getByText("Restored", { exact: true })).toBeVisible();
  const history = await world.api<unknown[]>("instance.history", { instance: id, online: false });
  expect(history.length).toBeGreaterThanOrEqual(3); // the unsaved change became a snapshot of its own
});
