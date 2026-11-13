import { card, expect, expectCardPhase, openApp, test } from "./world";

test("first run: start the PackSync cloud, preview an instance and start syncing", async ({ page, world }) => {
  await openApp(page, world);

  // nothing is connected and nothing synced: the app starts with the setup
  await expect(page.getByRole("heading", { name: "Your modpacks, on every computer" })).toBeVisible();
  await page.getByRole("button", { name: "Start on this computer" }).click();

  // step 2: Prism instances were found
  await expect(page.getByRole("heading", { name: "Choose what to sync" })).toBeVisible();
  const list = page.getByRole("list", { name: "Prism instances" });
  await expect(list.getByRole("listitem")).toHaveCount(2);
  await list.getByRole("listitem").filter({ hasText: "Skyblock Pack" }).getByRole("button", { name: /Preview and start/ }).click();

  // the preview says what is synced and what is not, and why
  const dialog = page.getByRole("dialog", { name: "Start syncing this instance" });
  await expect(dialog.getByText("files will be synced")).toBeVisible();
  await expect(dialog.getByRole("cell", { name: "Mods", exact: true })).toBeVisible();
  await expect(dialog.getByRole("heading", { name: "Not synced" })).toBeVisible();
  await expect(dialog.getByRole("cell", { name: "logs/" })).toBeVisible();
  await dialog.getByRole("button", { name: "Start syncing" }).click();
  await expect(dialog).toBeHidden();
  await expect(list.getByRole("listitem").filter({ hasText: "Skyblock Pack" }).getByText("Syncing")).toBeVisible();

  await page.getByRole("button", { name: "Finish" }).click();
  await expect(card(page, "Skyblock Pack")).toBeVisible();
  await expectCardPhase(page, "Skyblock Pack", "ready_on_other_devices");

  // and the engine agrees
  const list2 = await world.api<{ phase: string; name: string }[]>("instances.list", { online: false });
  expect(list2).toHaveLength(1);
  expect(list2[0]).toMatchObject({ name: "Skyblock Pack", phase: "ready_on_other_devices" });
});

test("the other Prism instance is offered under 'Not syncing yet' and can be added later", async ({ page, world }) => {
  await openApp(page, world);
  await page.getByRole("button", { name: "Start on this computer" }).click();
  await page.getByRole("button", { name: "Finish" }).click();

  await expect(page.getByText("No instances are synced yet")).toBeVisible();
  const section = page.getByRole("region", { name: "Instances that are not synced" });
  await expect(section.getByText("Vanilla Plus")).toBeVisible();
  await section.getByRole("listitem").filter({ hasText: "Vanilla Plus" }).getByRole("button", { name: /Start syncing/ }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Start syncing" }).click();
  await expect(page.getByRole("heading", { level: 1, name: "Vanilla Plus" })).toBeVisible();
});

test("Google Drive: without an OAuth client the app explains what is needed instead of failing", async ({ page, world }) => {
  await openApp(page, world);
  await page.getByRole("button", { name: "Use my own Google Drive instead" }).click();
  await expect(page.getByText(/unencrypted/)).toBeVisible();
  await page.getByRole("button", { name: "Sign in with Google" }).click();
  await expect(page.getByRole("heading", { name: "PackSync needs a Google OAuth client" })).toBeVisible();
  await expect(page.getByLabel("Downloaded client file (JSON)")).toBeVisible();
  // nothing was connected
  const st = await world.api<{ kind: string; connected: boolean }>("storage.status");
  expect(st.connected).toBe(false);
});
