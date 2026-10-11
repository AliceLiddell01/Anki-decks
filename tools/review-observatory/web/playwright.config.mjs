import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests",
  testMatch: "*.spec.mjs",
  fullyParallel: true,
  workers: 2,
  timeout: 30000,
  reporter: "list",
  outputDir: "../../../.anki-repo/observatory/browser-results",
  use: {
    baseURL: "http://127.0.0.1:4175",
    headless: true,
    launchOptions: { executablePath: process.env.CHROMIUM_PATH },
    trace: "retain-on-failure",
  },
  webServer: {
    command: "npm run dev -- --port 4175 --strictPort",
    wait: { stdout: /Local:.*4175/ },
    reuseExistingServer: false,
    timeout: 30000,
  },
});
