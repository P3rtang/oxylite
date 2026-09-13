import { defineConfig, devices } from "@playwright/test";

// The container stack command: podman locally, docker on CI (#39 —
// COMPOSE env). Same compose.yaml either way.
const compose = process.env.COMPOSE ?? "podman compose";

export default defineConfig({
  testDir: "./tests",
  timeout: 120_000,
  expect: { timeout: 20_000 },
  // Tests are isolation-safe by construction: every test uses its own
  // browser context (fresh IndexedDB = fresh PGlite = its own Web Locks
  // namespace) and asserts on unique Date.now() stamps. The REMOTE side
  // is shared, though: one Postgres serves every worker, so a fresh
  // context pulls every other test's rows too — assertions must always
  // filter by stamp, never count unfiltered rows (the accumulated test
  // junk also means ./test.sh --full --fresh is the honest baseline).
  // Workers are capped because each PGlite context is a wasm VM with a
  // 4GB heap budget; 3 keeps the 15s fresh-client asserts comfortable.
  fullyParallel: true,
  workers: 3,
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
      command: `DX=${process.env.DX_BIN ?? "$HOME/.cargo/bin/dx"} ../scripts/build-client.sh && ${compose} up -d && cargo run -p server`,
      url: "http://localhost:3000/health",
      reuseExistingServer: true,
      // The dist build (dx wasm) is slower on CI's 2-core runners than
      // locally — overridable via env (#39).
      timeout: Number(process.env.WEB_SERVER_TIMEOUT ?? 180_000),
    },
  ],
});
