import AxeBuilder from "@axe-core/playwright";
import { expect, openApp, test } from "./world";

test("the first-run setup is accessible", async ({ page, world }) => {
  await openApp(page, world);
  await expect(page.getByRole("heading", { name: "Continue your modpack on any computer" })).toBeVisible();
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  expect(results.violations.map((v) => `${v.id}: ${v.help}`)).toEqual([]);
});
