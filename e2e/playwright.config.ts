import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  timeout: 120_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  reporter: "list",
  use: {
    baseURL: "http://localhost:3000",
    headless: true,
    // PGlite needs a reasonable amount of memory for the wasm heap.
    contextOptions: {
      args: ["--js-flags=--max-old-space-size=4096"],
    },
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: [
    {
      // Build step for the e2e stack: the dist is rebuilt on every server
      // start (the serve path owns freshness — a reused server always
      // postdates its dist build), then Postgres must be up before the
      // server connects; folded into one command since Playwright only
      // supports http URLs for health checks.
      command:
        "DX=$HOME/.cargo/bin/dx ../scripts/build-client.sh && podman compose up -d && cargo run -p server",
      url: "http://localhost:3000/health",
      reuseExistingServer: true,
      timeout: 180_000,
    },
  ],
});
