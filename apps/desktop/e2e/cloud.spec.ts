import { NAME } from "./scenarios";
import { enableViaApi, expect, openApp, test } from "./world";

test.use({ connected: true });

test("bring an instance from another computer to this one", async ({ page, world }) => {
  await world.control({ cmd: "laptop_new_instance", instance: "Laptop Pack" });
  await openApp(page, world, "#/cloud");

  const c = page.getByRole("article", { name: "Laptop Pack" });
  await expect(c).toBeVisible();
  await expect(c.getByText("Last saved")).toContainText("Laptop");
  await c.getByRole("button", { name: "Bring to this computer…" }).click();

  const dialog = page.getByRole("dialog", { name: "Bring “Laptop Pack” to this computer" });
  await expect(dialog.getByLabel("Prism data folder")).toBeVisible();
  await dialog.getByRole("button", { name: "Bring it here" }).click();
  await expect(dialog.getByText("Done", { exact: true })).toBeVisible();
  await dialog.getByRole("button", { name: "Open the instance" }).click();
  await expect(page.getByRole("heading", { level: 1, name: "Laptop Pack" })).toBeVisible();

  // the instance is in the desktop's Prism and syncs from now on
  const found = await world.api<{ name: string; sync_enabled: boolean }[]>("launcher.discover");
  expect(found.find((d) => d.name === "Laptop Pack")).toMatchObject({ sync_enabled: true });
});

test("an instance that is already on this computer says so", async ({ page, world }) => {
  await enableViaApi(world, NAME);
  await openApp(page, world, "#/cloud");
  const c = page.getByRole("article", { name: NAME });
  await expect(c.getByText("On this computer")).toBeVisible();
  await expect(c.getByRole("link", { name: "Open it" })).toBeVisible();
});
