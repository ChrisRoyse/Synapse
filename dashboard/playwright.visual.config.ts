import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  metadata: {
    dockerImage: "mcr.microsoft.com/playwright:v1.60.0-noble"
  },
  testDir: "./tests",
  outputDir: "./test-results",
  reporter: [["list"]],
  fullyParallel: false,
  workers: 1,
  timeout: 45_000,
  expect: {
    timeout: 10_000,
    toHaveScreenshot: {
      maxDiffPixelRatio: 0.002,
      maxDiffPixels: 96
    }
  },
  use: {
    ...devices["Desktop Chrome"],
    baseURL: "http://127.0.0.1:6006",
    viewport: { width: 1280, height: 900 },
    colorScheme: "dark",
    screenshot: "only-on-failure",
    trace: "retain-on-failure"
  },
  webServer: {
    command: "bun run storybook -- --ci",
    url: "http://127.0.0.1:6006",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    stdout: "pipe",
    stderr: "pipe"
  },
  projects: [
    {
      name: "chromium-dashboard",
      use: { ...devices["Desktop Chrome"] }
    }
  ]
});
