import { expect, test, type Page } from "@playwright/test";

const NOTE_A = `e2e note alpha ${Date.now()}`;
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

test("add a note locally and persist across reload (PGlite)", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Offline Notes" })).toBeVisible();

  // PGlite boot can take a few seconds (wasm + IndexedDB).
  await expect(page.getByPlaceholder("Note title…")).toBeVisible({ timeout: 30_000 });
  await addNote(page, NOTE_A);

  // Reload: the note must come back from IndexedDB.
  await page.reload();
  await expect(page.getByRole("listitem").filter({ hasText: NOTE_A })).toBeVisible({
    timeout: 30_000,
  });
});

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
  // Reload so the app establishes a fresh (closed) connection.
  await page.reload();
  await expect(page.getByText(/offline/)).toBeVisible({ timeout: 30_000 });

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
