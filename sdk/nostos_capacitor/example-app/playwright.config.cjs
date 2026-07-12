// Playwright config for the @nostos-sync/capacitor live-replication E2E.
//
// Mirrors sdk/nostos_web/playwright.config.cjs. One project (chromium),
// headless. The spec spawns its own spine binary + static HTTP server (see
// e2e/push-echo.spec.cjs), so there's no globalSetup and no webServer — the
// test IS the lifecycle owner.
"use strict";

const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./e2e",
  timeout: 60000,
  expect: { timeout: 15000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    headless: true,
    browsers: ["chromium"],
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
});
