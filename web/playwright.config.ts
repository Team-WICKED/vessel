// End-to-end tests against a real fleet (e2e/fleet.ts), and the `screens`
// project that captures every main screen into artifacts/screens/
// (untracked) for review.
import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "e2e",
  outputDir: "test-results",
  globalSetup: "./e2e/global-setup.ts",
  globalTeardown: "./e2e/global-teardown.ts",
  // One fleet, shared: tests run one at a time.
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: "list",
  use: {
    ...devices["Desktop Chrome"],
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 1,
    colorScheme: "dark",
    // WS frames and navigation URLs can contain credentials. Never record them.
    trace: "off",
    screenshot: "off",
    video: "off",
  },
  projects: [
    { name: "e2e", testMatch: /\.spec\.ts$/ },
    { name: "screens", testMatch: /screens\.ts$/ },
  ],
});
