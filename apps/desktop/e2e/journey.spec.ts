import { card, enableInstance, enableViaApi, expect, expectCardPhase, expectDetailPhase, openApp, test, toast } from "./world";

test.use({ connected: true });

const STATS = "saves/World 0/stats/progress.json";

test("start syncing, play on the laptop, then continue on the desktop", async ({ page, world }) => {
  await openApp(page, world);
  await enableInstance(page, "Skyblock Pack");
  await expectDetailPhase(page, "ready_on_other_devices");

  // The laptop brings the instance over, plays, and uploads its progress.
  await world.control({ cmd: "laptop_restore", instance: "Skyblock Pack" });
  await world.control({ cmd: "edit", device: "laptop", instance: "Skyblock Pack", path: STATS, content: '{"day": 42}' });
  const rec = await world.control<{ outcome: string; uploaded: boolean }>({ cmd: "sync", device: "laptop", instance: "Skyblock Pack" });
  expect(rec).toMatchObject({ outcome: "local_ahead", uploaded: true });
  expect(world.hasInstanceFile("Skyblock Pack", STATS)).toBe(false);

  // Back on the desktop: "Sync now" brings the progress over.
  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Instances" }).click();
  await card(page, "Skyblock Pack").getByRole("button", { name: "Sync now" }).click();
  await expect(toast(page, /Applied newer progress from the cloud/)).toBeVisible();
  expect(world.readInstanceFile("Skyblock Pack", STATS)).toBe('{"day": 42}');

  // The history shows who saved what.
  await card(page, "Skyblock Pack").getByRole("link", { name: "Skyblock Pack" }).click();
  const snaps = page.getByRole("list", { name: "Snapshots" }).getByRole("listitem");
  await expect(snaps).toHaveCount(2);
  await expect(snaps.first()).toContainText("on Laptop");
  await expect(snaps.first()).toContainText("This computer");
});

test("Play: the session is saved and uploaded when the game closes", async ({ page, world }) => {
  const id = await enableViaApi(world, "Skyblock Pack");
  await world.control({ cmd: "fake_game", secs: 1.2, write: [STATS, '{"day": 7}'] });
  await openApp(page, world);

  const c = card(page, "Skyblock Pack");
  await expectCardPhase(page, "Skyblock Pack", "ready_on_other_devices");
  await c.getByRole("button", { name: "Play" }).click();
  await expect(c.getByRole("button", { name: "Playing…" })).toBeVisible();
  await expect(toast(page, "Session saved and uploaded.")).toBeVisible();
  await expect(c.getByRole("button", { name: "Play" })).toBeVisible();
  await expectCardPhase(page, "Skyblock Pack", "ready_on_other_devices");

  expect(world.readInstanceFile("Skyblock Pack", STATS)).toBe('{"day": 7}');
  const history = await world.api<{ state: string; kind: string }[]>("instance.history", { instance: id, online: false });
  expect(history).toHaveLength(2);
  expect(history.every((h) => h.kind === "normal")).toBe(true);
});

test("an upload that cannot finish is shown as 'saved here', never as synced", async ({ page, world }) => {
  const id = await enableViaApi(world, "Skyblock Pack");
  await openApp(page, world);
  // the cloud disappears (a disconnected network drive)
  await world.api("storage.disconnect", { revoke: false });
  await world.api("instance.snapshot", { instance: id, force: true });
  await page.reload();
  const c = card(page, "Skyblock Pack");
  await expectCardPhase(page, "Skyblock Pack", "local_snapshot_complete");
  await expect(c.getByText("Saved here, not uploaded yet")).toBeVisible();
  await expect(c.getByRole("list", { name: "Upload progress" })).toBeVisible();
});
