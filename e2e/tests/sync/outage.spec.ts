import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

// The stream-failure surface: when the server can no longer READ the
// sync log, the session must END — the client's reconnect machinery is
// the error surface (the status line shows the truth: offline), never a
// healthy-looking starved stream. Both halves of the socket loop obey
// the rule: the ticker sends Dead on a failed pull, and on_text breaks
// the session on Sql-level errors (Json keeps the connection — one
// poisoned message must not kill the stream).
//
// Tagged @isolated: the trigger is stopping the SHARED Postgres, so this
// spec runs in its own single-worker pass (see scripts/e2e.sh) — never
// alongside other workers' tests.

function psql(sql: string) {
  execSync(`podman compose exec -T postgres psql -U sync -d offline_notes -c "${sql}"`, {
    cwd: "..", // playwright runs from e2e/; compose file lives at the repo root
  });
}

function stopPg() {
  execSync("podman compose stop postgres", { cwd: "..", stdio: "ignore" });
}

function startPg() {
  execSync("podman compose start postgres", { cwd: "..", stdio: "ignore" });
}

function pgReady(): boolean {
  try {
    execSync("podman compose exec -T postgres pg_isready -U sync -d offline_notes", {
      cwd: "..",
      stdio: ["ignore", "pipe", "ignore"],
    });
    return true;
  } catch {
    return false;
  }
}

test.afterEach(() => {
  // Never leave the shared DB down, even on failure — this spec runs
  // last, but a retried or debugged run must start from a live stack.
  if (!pgReady()) startPg();
});

async function waitConnected(page: Page) {
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

test("a server that cannot read the sync log ends the session — the client goes offline and recovers @isolated", async ({
  page,
}) => {
  const canary = `e2e outage control ${Date.now()}`;
  psql(
    `INSERT INTO notes (id, title, body, updated_at)` +
      ` VALUES (gen_random_uuid(), '${canary}', '', now());` +
      ` INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
      ` SELECT 'notes', id, jsonb_build_object('id', id, 'title', title, 'body', body, 'updated_at', updated_at), updated_at` +
      ` FROM notes WHERE title = '${canary}';`,
  );

  await page.goto("/");
  await waitConnected(page);
  // Live and demonstrably synced before the outage.
  await expect(page.getByRole("listitem").filter({ hasText: canary })).toBeVisible();

  stopPg();
  // The ticker's next pull can neither succeed nor fail cleanly — the
  // stopped container black-holes established pool connections, so the
  // query HANGS. The transport's 2s bound turns the hang into a stream
  // failure (and a reconnect's Pull errors at the session too): both
  // end the session, the reconnect loop cannot reach the DB, and the
  // truth shows as offline. Before the bound this was the worst kind
  // of silent: a healthy-looking "connected" with a starved stream.
  await expect(page.locator("p").filter({ hasText: /offline/ })).toBeVisible({
    timeout: 20_000,
  });

  startPg();
  await expect.poll(pgReady, { timeout: 30_000, interval: 1_000 }).toBe(true);
  // The next reconnect sticks: connected again, the canary still there.
  await waitConnected(page);
  await expect(page.getByRole("listitem").filter({ hasText: canary })).toBeVisible();
});
