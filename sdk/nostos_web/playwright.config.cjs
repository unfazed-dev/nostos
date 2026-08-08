// Playwright config for the @nostos-sync/web live-replication E2E.
//
// One project (chromium), headless. The spec spawns its own spine binary +
// static HTTP server (see browser_live.spec.cjs), so there's no globalSetup
// and no webServer — the test IS the lifecycle owner.
"use strict";

const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./e2e",
  // attachments.spec.cjs is a `node:test` file (run via `npm run smoke`),
  // NOT a Playwright spec — exclude it so Playwright doesn't side-effect
  // execute it during discovery (it would otherwise run twice under `npm test`).
  testIgnore: ["**/attachments.spec.cjs"],
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
