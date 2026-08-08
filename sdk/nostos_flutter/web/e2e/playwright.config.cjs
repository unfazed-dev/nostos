// Playwright config for the Flutter-web Worker browser smoke (ADR-0036).
// One project (chromium, headless), the spec spawns its own spine + static server.
"use strict";
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./",
  testMatch: /flutter_web_smoke\.spec\.cjs/,
  timeout: 90000,
  expect: { timeout: 20000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: { headless: true },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
