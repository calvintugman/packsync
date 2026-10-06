import { NAME } from "./scenarios";
import { card, enableViaApi, expect, expectCardPhase, openApp, test, toast } from "./world";

test.use({ connected: true });

const PLAYED = "saves/World 0/stats/played-offline.json";

test("the cloud is unreachable and the policy is 'don't start': Play explains and offers the explicit choice", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await world.api("settings.update", { patch: { hook_offline_policy: "block" } });
  await world.control({ cmd: "cloud_mode", mode: "offline" });
  await world.control({ cmd: "fake_game", secs: 0.8, write: [PLAYED, "{}"] });
  await openApp(page, world);

  const c = card(page, NAME);
  await c.getByRole("button", { name: "Play" }).click();
  await expect(c.getByText("Can't reach the cloud")).toBeVisible();
  await expect(c.getByRole("button", { name: "Try again" })).toBeVisible();
  expect(world.hasInstanceFile(NAME, PLAYED)).toBe(false); // the game did not start

  // the person chooses to play the local copy
  await c.getByRole("button", { name: "Play from this computer anyway" }).click();
  await expect(toast(page, /Played offline/)).toBeVisible();
  expect(world.hasInstanceFile(NAME, PLAYED)).toBe(true);
  // honest status: saved here, not uploaded
  await expectCardPhase(page, NAME, "local_snapshot_complete");
  await expect(c.getByText("Saved here, not uploaded yet")).toBeVisible();

  // back online: nothing is lost, and the progress reaches the cloud
  await world.control({ cmd: "cloud_mode", mode: "ok" });
  await c.getByRole("button", { name: "Sync now" }).click();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
  const history = await world.api<unknown[]>("instance.history", { instance: id, online: false });
  expect(history).toHaveLength(2);
});

test("the default policy plays the local copy when the cloud is unreachable, and says so", async ({ page, world }) => {
  await enableViaApi(world, NAME);
  await world.control({ cmd: "cloud_mode", mode: "offline" });
  await world.control({ cmd: "fake_game", secs: 0.8, write: [PLAYED, "{}"] });
  await openApp(page, world);

  const c = card(page, NAME);
  await c.getByRole("button", { name: "Play" }).click();
  await expect(toast(page, /The cloud could not be reached|The cloud is not available/)).toBeVisible();
  await expect(toast(page, /Played offline/)).toBeVisible();
  expect(world.hasInstanceFile(NAME, PLAYED)).toBe(true);
  await expectCardPhase(page, NAME, "local_snapshot_complete");
});

test("a full Google Drive is explained, the work stays safe, and Upload now retries at once", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await world.control({ cmd: "cloud_mode", mode: "quota" });
  await world.control({ cmd: "edit", device: "desktop", instance: NAME, path: "config/big-change.toml", content: "x = 1\n" });
  await openApp(page, world);

  const c = card(page, NAME);
  await c.getByRole("button", { name: "Sync now" }).click();
  // the problem is explained next to the button, in words, with what to do
  const problem = c.getByRole("alert");
  await expect(problem).toContainText("Google Drive is full");
  await expect(problem).toContainText("local data is unaffected");
  // never claimed as synced
  await expect(c).toHaveAttribute("data-phase", /upload_pending|local_snapshot_complete/);
  await expect(c.getByTestId("pending")).toContainText("waiting to upload");

  // Drive has room again: the person asks for the upload now, even though a back-off was recorded
  await world.control({ cmd: "cloud_mode", mode: "ok" });
  await c.getByRole("button", { name: "Upload now" }).click();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
  expect(await world.api<unknown[]>("instance.history", { instance: id, online: false })).toHaveLength(2);
});

test("a revoked sign-in says so and points to Settings", async ({ page, world }) => {
  await enableViaApi(world, NAME);
  await world.control({ cmd: "cloud_mode", mode: "auth" });
  await openApp(page, world);
  const c = card(page, NAME);
  await c.getByRole("button", { name: "Sync now" }).click();
  await expect(c.getByText("Access was revoked", { exact: true })).toBeVisible();
  await c.getByRole("button", { name: "Sign in again" }).click();
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
});

test("a slow upload can be cancelled and nothing half-finished is shown as synced", async ({ page, world }) => {
  await enableViaApi(world, NAME);
  await world.control({ cmd: "cloud_mode", mode: "slow", ms: 4000 });
  await world.control({ cmd: "edit", device: "desktop", instance: NAME, path: "config/slow.toml", content: "x = 2\n" });
  await openApp(page, world);

  const c = card(page, NAME);
  await c.getByRole("button", { name: "Sync now" }).click();
  const cancel = c.getByRole("button", { name: "Cancel" });
  await expect(cancel).toBeVisible();
  await cancel.click();
  await expect(cancel).toBeHidden();
  await expect(c).toHaveAttribute("data-phase", /upload_pending|local_snapshot_complete/);
  await expect(c.getByRole("button", { name: "Sync now" })).toBeEnabled();

  // and it can be retried from the activity list
  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: /Activity/ }).click();
  const row = page.getByRole("listitem").filter({ hasText: /Syncing Skyblock Pack/ }).first();
  await expect(row.getByText("Cancelled")).toBeVisible();
  await world.control({ cmd: "cloud_mode", mode: "ok" });
  await row.getByRole("button", { name: "Try again" }).click();
  await expect(toast(page, "Done.")).toBeVisible();
  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Instances" }).click();
  await expectCardPhase(page, NAME, "ready_on_other_devices");
});
