import { expect, test, type Page } from "@playwright/test";

// Queue durability (#26, docs/impl/multi-tab.md): the engine's pending queue
// is in-memory (`RefCell<Vec<Op>>`, engine.rs) and dies with the tab — writes
// committed while offline never enter sync_log unless the SAME engine
// reconnects. These tests state the durability contract the fix must
// satisfy; all three are intentionally red until the pending queue is
// durable (no test.fail(): red is the goal, per the #23 convention — the
// fix flips them green). They cover the three ways an engine dies with
// unsynced writes:
//   1. page reload (single-tab variant: the queue dies with the engine)
//   2. tab close while a sibling tab survives (handoff must flush)
//   3. every tab closes; a later tab must resume the durable queue

const RELOAD_NOTE = `e2e reload note ${Date.now()}`;
const SURVIVOR_NOTE = `e2e survivor note ${Date.now()}`;
const TAB_A_NOTE = `e2e tab-a note ${Date.now()}`;
const TAB_B_NOTE = `e2e tab-b note ${Date.now()}`;

async function boot(page: Page) {
  await page.goto("/");
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
}

// Route the sync socket to a hard close and reload, so the engine
// reconnects into a dead socket. "offline — will retry…" proves a connect
// attempt already failed, so the next write queues in pending (not
// "offline — local data loaded", which shows before the first attempt).
async function goOffline(page: Page) {
  await page.routeWebSocket("**/sync", (ws) => ws.close());
  await page.reload();
  await expect(page.locator("p").filter({ hasText: /will retry/ })).toBeVisible({
    timeout: 30_000,
  });
}

test("a note written offline survives a reload and reaches a fresh client", async ({ page, browser }) => {
  await boot(page);

  await goOffline(page);

  // The write commits locally (IndexedDB) and queues in the in-memory
  // pending — the push never reaches the server.
  await addNote(page, RELOAD_NOTE);

  // Reload: the engine — and with it the whole pending queue — is destroyed
  // and rebuilt. The row survives in IndexedDB; the queued op must too.
  await page.reload();
  await expect(page.getByRole("listitem").filter({ hasText: RELOAD_NOTE })).toBeVisible({
    timeout: 30_000,
  });

  // Unblock the socket: the rebuilt engine reconnects on its 3s backoff and
  // must flush what was committed before the reload.
  await page.routeWebSocket("**/sync", (ws) => ws.connectToServer());
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });

  // The sync contract: a fresh client (fresh IndexedDB) must eventually
  // receive every local write.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: RELOAD_NOTE })).toBeVisible({
    timeout: 15_000,
  });
  await ctx2.close();
});

test("a note written offline in one tab reaches a fresh client after that tab closes", async ({ browser }) => {
  // A follower's write may not die with the follower (impl/multi-tab.md):
  // the surviving tab must take over the dead tab's unsynced write.
  const ctx = await browser.newContext();
  const tabA = await ctx.newPage();
  await boot(tabA);
  const tabB = await ctx.newPage();
  await boot(tabB);

  // tabB goes offline and writes; tabA stays online the whole time.
  await goOffline(tabB);
  await addNote(tabB, SURVIVOR_NOTE);

  // tabB closes — engine, socket and in-memory pending die here.
  await tabB.close();

  // The contract: the write reaches the server anyway (the surviving tab
  // flushes the shared queue), so a fresh client must eventually see it.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: SURVIVOR_NOTE })).toBeVisible({
    timeout: 15_000,
  });
  await ctx2.close();
  await ctx.close();
});

test("notes written offline in two tabs reach a fresh client after both tabs close", async ({ browser }) => {
  // Each engine dies with unsynced writes; a later tab in the same context
  // must resume the durable queue and flush both. Engine lifetimes are
  // sequential on purpose: two LIVE instances writing the same IndexedDB
  // lose writes at the storage layer (later writer clobbers the earlier
  // one's committed row — observed in the first run of this spec, see the
  // audit finding in docs/impl/multi-tab.md). Single-writer ownership is
  // part of the fix, not this test.
  const ctx = await browser.newContext();
  const tabA = await ctx.newPage();
  await boot(tabA);
  await goOffline(tabA);
  await addNote(tabA, TAB_A_NOTE);
  await tabA.close();

  const tabB = await ctx.newPage();
  await boot(tabB);
  await goOffline(tabB);
  await addNote(tabB, TAB_B_NOTE);
  await tabB.close();

  // A later tab resumes the context: it boots from the shared IndexedDB and
  // must flush whatever earlier tabs left unsynced.
  const tabC = await ctx.newPage();
  await boot(tabC);
  await expect(tabC.getByRole("listitem").filter({ hasText: TAB_A_NOTE })).toBeVisible();
  await expect(tabC.getByRole("listitem").filter({ hasText: TAB_B_NOTE })).toBeVisible();

  // The sync contract: a fresh client must receive both writes.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: TAB_A_NOTE })).toBeVisible({
    timeout: 15_000,
  });
  await expect(other.getByRole("listitem").filter({ hasText: TAB_B_NOTE })).toBeVisible({
    timeout: 15_000,
  });
  await ctx2.close();
  await ctx.close();
});
