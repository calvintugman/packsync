import { readFileSync } from "node:fs";
import { join } from "node:path";
import { NAME } from "./scenarios";
import { enableViaApi, expect, openApp, test, toast } from "./world";

test.use({ connected: true });

test("launcher hooks: preview the exact change, install, remove", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  const cfg = () => readFileSync(join(world.instanceRoot(NAME), "instance.cfg"), "utf8");
  await openApp(page, world, `#/instance/${id}/hooks`);

  await expect(page.getByText("Not installed")).toBeVisible();
  await page.getByRole("button", { name: "Install hooks…" }).click();
  const dialog = page.getByRole("dialog", { name: "Install launcher hooks" });
  await expect(dialog.getByText("PreLaunchCommand")).toBeVisible();
  await expect(dialog.getByText("PostExitCommand")).toBeVisible();
  await expect(dialog.getByLabel("PreLaunchCommand after")).toContainText("hook pre-launch");
  // nothing is written until the person confirms
  expect(cfg()).not.toContain("PreLaunchCommand");
  await dialog.getByRole("button", { name: "Install hooks" }).click();
  await expect(toast(page, "Launcher hooks installed.")).toBeVisible();
  await expect(page.getByText("Installed", { exact: true })).toBeVisible();
  expect(cfg()).toContain("PreLaunchCommand=");
  expect(cfg()).toContain("hook post-exit");

  await page.getByRole("button", { name: "Remove hooks…" }).click();
  const remove = page.getByRole("dialog", { name: "Remove launcher hooks" });
  await expect(remove.getByLabel("PreLaunchCommand before")).toContainText("hook pre-launch");
  await remove.getByRole("button", { name: "Remove hooks" }).click();
  await expect(toast(page, "Launcher hooks removed.")).toBeVisible();
  await expect(page.getByText("Not installed")).toBeVisible();
  expect(cfg()).not.toContain("hook pre-launch");
});

test("hooks chain the user's own commands instead of replacing them", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  const path = join(world.instanceRoot(NAME), "instance.cfg");
  const { appendFileSync } = await import("node:fs");
  appendFileSync(path, "OverrideCommands=true\nPreLaunchCommand=echo my-own-command\n");
  await openApp(page, world, `#/instance/${id}/hooks`);
  await page.getByRole("button", { name: "Install hooks…" }).click();
  const dialog = page.getByRole("dialog", { name: "Install launcher hooks" });
  await expect(dialog.getByText("Your own commands keep running")).toBeVisible();
  await expect(dialog.getByText("pre-launch: echo my-own-command")).toBeVisible();
  await dialog.getByRole("button", { name: "Install hooks" }).click();
  await expect(page.getByText("Installed", { exact: true })).toBeVisible();
  expect(readFileSync(path, "utf8")).toContain("echo my-own-command");
});

test("a hook command that was replaced is reported as partly installed and can be repaired", async ({ page, world }) => {
  const id = await enableViaApi(world, NAME);
  await openApp(page, world, `#/instance/${id}/hooks`);
  await page.getByRole("button", { name: "Install hooks…" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Install hooks" }).click();
  await expect(page.getByText("Installed", { exact: true })).toBeVisible();

  // something rewrites the command behind PackSync's back
  const path = join(world.instanceRoot(NAME), "instance.cfg");
  const { writeFileSync } = await import("node:fs");
  writeFileSync(path, readFileSync(path, "utf8").replace(/PreLaunchCommand=.*/, "PreLaunchCommand=echo something-else"));
  await page.reload();
  await expect(page.getByText("Partly installed")).toBeVisible();
  await page.getByRole("button", { name: "Repair hooks…" }).click();
  const dialog = page.getByRole("dialog", { name: "Install launcher hooks" });
  // the replacement command is the person's own now, so it keeps running after PackSync's part
  await expect(dialog.getByText("pre-launch: echo something-else")).toBeVisible();
  await dialog.getByRole("button", { name: "Install hooks" }).click();
  await expect(page.getByText("Installed", { exact: true })).toBeVisible();
  expect(readFileSync(path, "utf8")).toContain("echo something-else");
  expect(readFileSync(path, "utf8")).toContain("hook pre-launch");
});
