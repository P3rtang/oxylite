import { expect, test, type Page } from "@playwright/test";

// Multi-tab (docs/impl/multi-tab.md): the engine is browser-wide now.
// One tab — the leader — holds a Web Lock, the single PGlite instance and
// the websocket; every other tab is a subordinate that proxies its DB
// access over BroadcastChannel and never opens PGlite (two live instances
// over one IndexedDB lose writes). A subordinate's write is forwarded to
// the leader's queue, so the writing tab dying no longer loses it — the
// test below proves that contract. What still loses writes is LEADER
// death while the queue is in-memory: a reload (single tab) and an
// all-tabs-close are queue.spec.ts's durability reds.

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

test("two tabs share the DB and propagate writes through the server", async ({ browser }) => {
  // Control: same-context tabs sync via the server roundtrip (ticker pull).
  // Local echo also lights the other tab up immediately via BroadcastChannel;
  // the assertion is deliberately roundtrip-tolerant.
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

test("an offline write in a tab that closes before reconnect still reaches other clients", async ({ browser }) => {
  // The engine holds every tab's writes: the write below happens in a
  // SUBORDINATE tab and is forwarded to the leader's queue, so the writing
  // tab dying no longer loses it. (What still loses writes: the leader
  // dying while the queue is in-memory — queue.spec.ts's durability reds.)

  // One context = shared IndexedDB: tabA and tabB see the same notes table.
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
  await addNote(writer, ZOMBIE);

  // The writing tab closes for good. The op survives: it lives in the
  // leader's queue.
  await writer.close();

  // A fresh tab in the SAME context still boots from the shared DB and
  // sees the note even before the reconnect flushes it.
  const tabC = await ctx.newPage();
  await tabC.goto("/");
  await expect(tabC.getByRole("listitem").filter({ hasText: ZOMBIE })).toBeVisible({
    timeout: 30_000,
  });

  // Unblock: the leader's reconnect flushes the queue.
  await goOnline([leader]);

  // The sync contract: a fresh client (fresh IndexedDB) must eventually
  // receive every local write.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: ZOMBIE })).toBeVisible({
    timeout: 15_000,
  });
  await ctx2.close();
  await ctx.close();
});
