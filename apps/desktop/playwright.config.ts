import { defineConfig } from "@playwright/test";

// Every test starts its own world (two computers, a shared cloud folder, a fake Prism and game) with the
// real application layer behind the real UI. See e2e/world.ts.
export default defineConfig({
  testDir: "e2e",
  globalSetup: "./e2e/global-setup.ts",
  timeout: 90_000,
  expect: { timeout: 15_000 },
  fullyParallel: true,
  workers: process.env.CI ? 2 : 3,
  retries: 0,
  forbidOnly: Boolean(process.env.CI),
  reporter: [["list"], ["html", { open: "never", outputFolder: "playwright-report" }]],
  use: {
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    viewport: { width: 1180, height: 800 },
    colorScheme: "light",
    locale: "en-US",
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
