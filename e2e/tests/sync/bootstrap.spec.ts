import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

const NOTE_B = `e2e note offline ${Date.now()}`;

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
}

async function waitForNote(page: Page, title: string) {
  await expect(
    page.getByRole("listitem").filter({ hasText: title }),
  ).toBeVisible({ timeout: 30_000 });
}

// Sync lifecycle: first push/pull over the websocket, offline queue flush
// on reconnect, and cold-client snapshot bootstrap.

test("syncs to a second client over websocket", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });

  const title = `e2e sync note ${Date.now()}`;
  await addNote(page, title);

  // A completely fresh browser context = fresh PGlite = must pull from server.
  const ctxB = await page.context().browser()!.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await waitForNote(pageB, title);
  await ctxB.close();
});

test("writes while offline are queued and flushed on reconnect", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });

  // Intercept the sync socket and force-close it: the app must show offline
  // and queue writes (Chrome keeps established sockets open in offline mode,
  // so this simulates a dead connection instead).
  await page.routeWebSocket("**/sync", (ws) => ws.close());
  await page.routeWebSocket("**/sync", (ws) => ws.close());
  // Reload so the app establishes a fresh (closed) connection. Scoped to
  // the status <p> — note titles may contain "offline" too (strict mode).
  await page.reload();
  await expect(page.locator("p").filter({ hasText: /offline/ })).toBeVisible({
    timeout: 30_000,
  });

  const noteB = `${NOTE_B}-queued`;
  await page.getByPlaceholder("Note title…").fill(noteB);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: noteB })).toBeVisible();

  // Unblock: re-route the socket with a pass-through handler (the newer
  // registration wins) and wait for the reconnect (3s backoff) to flush.
  await page.routeWebSocket("**/sync", (ws) => ws.connectToServer());

  const ctxB = await page.context().browser()!.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await waitForNote(pageB, noteB);
  await ctxB.close();
});

test("cold client bulk-loads a snapshot instead of replaying row by row", async ({ browser }) => {
  // Seed 120 notes straight into Postgres (notes + sync_log, the same shape
  // push() writes). With more than SNAPSHOT_AFTER_OPS=50 log entries, a
  // fresh client must be served the bulk snapshot, not 120 upsert replays.
  const stamp = `snapseed ${Date.now()}`;
  // The to_char expression builds canonical ISO text (`…Z`), but it is a
  // TYPED text expression — timestamptz columns (migration 0007) reject
  // it without an explicit cast (string literals coerce, expressions
  // don't). The storage layer doing the validating is the point.
  const sql =
    `INSERT INTO notes (id, title, body, updated_at)` +
    ` SELECT gen_random_uuid(), '${stamp} ' || g, '',` +
    ` (to_char(now() - (g || ' seconds')::interval, 'YYYY-MM-DD') || 'T' ||` +
    ` to_char(now() - (g || ' seconds')::interval, 'HH24:MI:SS.MS') || 'Z')::timestamptz` +
    ` FROM generate_series(1, 120) g;` +
    ` INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
    ` SELECT 'notes', id, jsonb_build_object('id', id, 'title', title, 'body', body, 'updated_at', updated_at), updated_at` +
    ` FROM notes WHERE title LIKE '${stamp}%';`;
  execSync(`podman compose exec -T postgres psql -U sync -d offline_notes -c "${sql}"`, {
    cwd: "..", // playwright runs from e2e/; compose file lives at the repo root
  });

  // A completely fresh context = empty IndexedDB = cold bootstrap.
  const ctx = await browser.newContext();
  const cold = await ctx.newPage();
  await cold.goto("/");
  const seeded = cold.getByRole("listitem").filter({ hasText: stamp });
  await expect(seeded).toHaveCount(120, { timeout: 30_000 });
  await ctx.close();
});
