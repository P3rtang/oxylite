import { expect, test, type Page } from "@playwright/test";

// Queue durability (#26, docs/impl/multi-tab.md): the browser's single
// engine (the leader tab) holds every tab's writes — but its pending
// queue is in-memory (`RefCell<Vec<Op>>`, engine.rs) and dies with the
// tab. Writes committed while offline never enter sync_log unless the
// same leader reconnects. These tests state the durability contract the
// fix must satisfy; tests 1 and 3 are still intentionally red (no
// test.fail(): red is the goal, per the #23 convention — the durable
// queue flips them green). Test 2 went green with the leader/follower
// engine: a subordinate's write survives the WRITER dying, because the
// leader holds it. They cover the three ways the browser loses unsynced
// writes:
//   1. page reload of the leader (single-tab variant: the queue dies
//      with the engine)
//   2. (green) the writing tab is a subordinate and closes — the leader
//      holds the op
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

// Make the whole browser offline. Two Playwright facts shape this:
// (1) only the leader tab owns a socket, and a leader reload hands the
// Web Lock to a queued tab — leadership churns through reloads; (2) a
// socket route only arms in documents created AFTER the registration (a
// late registration is inert on the live page — see PLAN.md gotchas), so
// every tab is reloaded after its close-route is registered and whoever
// ends up leading reconnects into an armed route. The offline status is
// browser-wide on every tab.
async function goOffline(pages: Page[]) {
  for (const page of pages) {
    await page.routeWebSocket("**/sync", (ws) => ws.close());
  }
  for (const page of pages) {
    await page.reload();
  }
  for (const page of pages) {
    await expect(page.locator("p").filter({ hasText: /will retry/ })).toBeVisible({
      timeout: 30_000,
    });
  }
}

// Undo it: a fresh (late) registration disarms the close-routes and is
// itself inert on the live pages, so the leader's reconnect goes through
// raw and the flush happens.
async function goOnline(pages: Page[]) {
  for (const page of pages) {
    await page.routeWebSocket("**/sync", (ws) => ws.connectToServer());
  }
}

// After goOffline the leader is whichever tab won the promotion race; the
// follower renders the relayed status with a role marker.
async function followerOf(pages: Page[]): Promise<Page> {
  const aSubordinate = await pages[0]
    .locator("p")
    .filter({ hasText: /subordinate/ })
    .isVisible();
  return aSubordinate ? pages[0] : pages[1];
}

test("a note written offline survives a reload and reaches a fresh client", async ({ page, browser }) => {
  await boot(page);

  await goOffline([page]);

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
  // GREEN since the leader/follower engine: a subordinate's write is
  // forwarded to the leader's queue, so the writing tab dying no longer
  // loses it. (Leader death is still lossy until the queue is durable —
  // the tests above and below.)
  const ctx = await browser.newContext();
  const tabA = await ctx.newPage();
  await boot(tabA);
  const tabB = await ctx.newPage();
  await boot(tabB);

  // Browser-wide offline: every tab reloads into its armed close-route;
  // leadership churns and whoever ends up leading is offline.
  await goOffline([tabA, tabB]);

  // The write goes in through the SUBORDINATE tab, so it lands in the
  // leader's queue, not in the writer's memory.
  const writer = await followerOf([tabA, tabB]);
  const leader = writer === tabA ? tabB : tabA;
  await addNote(writer, SURVIVOR_NOTE);

  // The writing tab closes for good. The op survives: it lives in the
  // leader's queue.
  await writer.close();

  // Unblock: the leader's reconnect flushes the queue.
  await goOnline([leader]);

  // The sync contract: a fresh client must eventually see it.
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
  // audit finding in docs/impl/multi-tab.md). Post leader/follower engine
  // only the leader ever opens PGlite, but the test keeps sequential tabs
  // so it isolates queue durability regardless of role churn.
  const ctx = await browser.newContext();
  const tabA = await ctx.newPage();
  await boot(tabA);
  await goOffline([tabA]);
  await addNote(tabA, TAB_A_NOTE);
  await tabA.close();

  const tabB = await ctx.newPage();
  await boot(tabB);
  await goOffline([tabB]);
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
