import AxeBuilder from "@axe-core/playwright";
import { expect, openApp, test } from "./world";

// check the settled screen: entry animations fade elements in, and mid-fade text is measured as faint
test.use({ reducedMotion: "reduce" });

test("the first-run setup is accessible", async ({ page, world }) => {
  await openApp(page, world);
  await expect(page.getByRole("heading", { name: "Your modpacks, on every computer" })).toBeVisible();
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
  expect(results.violations.map((v) => `${v.id}: ${v.help}`)).toEqual([]);
});
