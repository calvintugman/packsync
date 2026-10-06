import { NAME } from "./scenarios";
import { enableViaApi, expect, openApp, test, toast } from "./world";

test.use({ connected: true });

test("rename this computer", async ({ page, world }) => {
  await openApp(page, world, "#/settings");
  const section = page.getByRole("region", { name: "This computer" });
  await section.getByLabel("Name").fill("Study PC");
  await section.getByRole("button", { name: "Rename" }).click();
  await expect(toast(page, /Renamed/)).toBeVisible();
  await expect(page.getByLabel("Connection")).toContainText("Study PC");
  const info = await world.api<{ device: { name: string } }>("app.info");
  expect(info.device.name).toBe("Study PC");
});

test("play-from-Prism settings are saved", async ({ page, world }) => {
  await openApp(page, world, "#/settings");
  await page.getByRole("radio", { name: /Don't start the game/ }).check();
  await page.getByRole("checkbox", { name: /Don't start the game while a conflict/ }).uncheck();
  await expect.poll(async () => (await world.api<{ hook_offline_policy: string; hook_block_on_problem: boolean }>("settings.get"))).toMatchObject({
    hook_offline_policy: "block",
    hook_block_on_problem: false,
  });
  await page.reload();
  await expect(page.getByRole("radio", { name: /Don't start the game/ })).toBeChecked();
});

test("advanced settings are validated by the engine and the error is shown", async ({ page, world }) => {
  await openApp(page, world, "#/settings");
  const section = page.getByRole("region", { name: "Advanced" });
  await section.getByLabel("Parallel uploads").fill("0");
  await section.getByRole("button", { name: "Save" }).click();
  await expect(section.getByRole("alert")).toBeVisible();
  await section.getByLabel("Parallel uploads").fill("6");
  await section.getByRole("button", { name: "Save" }).click();
  await expect(toast(page, "Saved.")).toBeVisible();
  const s = await world.api<{ upload_concurrency: number }>("settings.get");
  expect(s.upload_concurrency).toBe(6);
});

test("test the connection, run diagnostics, plan a cleanup", async ({ page, world }) => {
  await enableViaApi(world, NAME);
  await openApp(page, world, "#/settings");
  await page.getByRole("button", { name: "Test the connection" }).click();
  await expect(page.getByText("The connection works")).toBeVisible();

  await page.getByRole("button", { name: "Run checks", exact: true }).click();
  const checks = page.getByRole("table", { name: "Checks" });
  await expect(checks.getByRole("row", { name: /storage/ })).toBeVisible();
  await expect(checks.getByRole("row", { name: /Prism Launcher/ })).toBeVisible();

  await page.getByRole("button", { name: "Clean up the cloud…" }).click();
  const dialog = page.getByRole("dialog", { name: "Clean up the cloud" });
  await dialog.getByRole("button", { name: "Make a plan" }).click();
  await expect(dialog.getByText("Nothing to clean up")).toBeVisible();
  await expect(dialog.getByRole("button", { name: "Run this plan" })).toBeDisabled();
});

test("disconnecting forgets the connection but never deletes anything", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await openApp(page, world, "#/settings");
  await page.getByRole("button", { name: "Disconnect…" }).click();
  const dialog = page.getByRole("dialog", { name: "Disconnect from the cloud?" });
  await expect(dialog.getByText("Nothing is deleted")).toBeVisible();
  await dialog.getByRole("button", { name: "Disconnect" }).click();
  await expect(page.getByLabel("Connection")).toContainText("Not connected");
  const st = await world.api<{ kind: string }>("storage.status");
  expect(st.kind).toBe("none");
  // the instance and its local snapshots are still there
  const list = await world.api<{ id: string }[]>("instances.list", {});
  expect(list.map((i) => i.id)).toContain(id);
  expect(world.hasInstanceFile(NAME, "config")).toBe(true);
});
