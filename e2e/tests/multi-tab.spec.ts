import { expect, test, type Page } from "@playwright/test";

// Multi-tab audit (docs/impl/multi-tab.md): two tabs in ONE browser context
// share the same IndexedDB (same PGlite data dir, same meta cursor) but each
// tab boots its OWN engine with its OWN in-memory pending queue.
//
// Confirmed break: push() queues into an in-memory RefCell<Vec<Op>> that is
// only drained on connect. A tab that writes while offline and closes before
// reconnecting loses the queued ops forever — the row exists in the shared
// IndexedDB (a fresh tab in the same context still sees it) but was never
// pushed: it never enters sync_log, so no other client ever receives it.
// Silent cross-client data loss, 100% reproducible.

const ZOMBIE = `e2e zombie note ${Date.now()}`;

async function boot(page: Page) {
  await page.goto("/");
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
}

test("two tabs share the DB and propagate writes through the server", async ({ browser }) => {
  // Control: same-context tabs sync via the server roundtrip (ticker pull).
  const ctx = await browser.newContext();
  const tabA = await ctx.newPage();
  await boot(tabA);
  const tabB = await ctx.newPage();
  await boot(tabB);

  const title = `e2e cross-tab ${Date.now()}`;
  await addNote(tabA, title);
  await expect(tabB.getByRole("listitem").filter({ hasText: title })).toBeVisible({
    timeout: 15_000,
  });
  await ctx.close();
});

test("an offline write in a tab that closes before reconnect is lost to other clients", async ({ browser }) => {
  // INTENTIONALLY BREAKING until the pending queue is durable (see
  // docs/impl/multi-tab.md): a tab that writes offline and closes before
  // reconnecting loses the op forever — the fresh-client assertion below
  // fails until then.

  // One context = shared IndexedDB: tabA and tabB see the same notes table.
  const ctx = await browser.newContext();
  const tabA = await ctx.newPage();
  await boot(tabA);
  const tabB = await ctx.newPage();
  await boot(tabB);

  // Kill tabB's socket: route its /sync connection to a hard close, then
  // reload so its engine reconnects into the dead socket. "offline — will
  // retry…" proves a connect attempt already failed, so the next write
  // queues in pending (not "offline — local data loaded", which shows
  // before the first attempt).
  await tabB.routeWebSocket("**/sync", (ws) => ws.close());
  await tabB.reload();
  await expect(tabB.locator("p").filter({ hasText: /will retry/ })).toBeVisible({
    timeout: 30_000,
  });

  // The write commits locally (shared IndexedDB) and queues in tabB's
  // in-memory pending — the push never reaches the server.
  await addNote(tabB, ZOMBIE);

  // Close tabB: its engine, socket and pending queue die here.
  await tabB.close();

  // tabA stayed online the whole time; its ticker pulls since the shared
  // cursor and finds nothing — the op never entered sync_log.
  // A fresh tab in the SAME context still boots from the shared DB, so the
  // zombie is visible locally. Smoking gun: the row exists but was never
  // synced.
  const tabC = await ctx.newPage();
  await boot(tabC);
  await expect(tabC.getByRole("listitem").filter({ hasText: ZOMBIE })).toBeVisible();

  // The sync contract: a fresh client (fresh IndexedDB) must eventually
  // receive every local write. It never does — this is the break.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: ZOMBIE })).toBeVisible({
    timeout: 15_000,
  });
  await ctx2.close();
  await ctx.close();
});
