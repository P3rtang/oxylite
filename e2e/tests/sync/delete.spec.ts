import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

// Delete/tombstone contracts (#27, docs/impl/deletes-tombstones.md): a
// delete is an op like any other (`data: null`), guarded by tombstones so
// stale writes cannot resurrect the row. The ✕ button drives the same
// path a remote delete takes. Seed notes ("seed: …") are the canary for
// "bootstrap worked" in every absence assertion — an empty list must
// never pass vacuously.

const DELETED_NOTE = `e2e doomed note ${Date.now()}`;
// Fixed timestamps bracket the wall-clock stamps the UI generates: the
// stale edit loses to the delete, the resurrecting edit wins against it.
const PAST = "2026-01-01T00:00:00.000Z";
const FUTURE = "2027-01-01T00:00:00.000Z";

async function boot(page: Page) {
  await page.goto("/");
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
}

async function deleteNote(page: Page, title: string) {
  const item = page.getByRole("listitem").filter({ hasText: title });
  await item.getByRole("button", { name: "✕" }).click();
  await expect(item).toHaveCount(0);
}

function psql(sql: string) {
  execSync(`podman compose exec -T postgres psql -U sync -d offline_notes -c "${sql}"`, {
    cwd: "..", // playwright runs from e2e/; compose file lives at the repo root
  });
}

// A note op seeded straight into sync_log (the same shape push() logs).
// The id comes from the op history (`rowIdByTitle`), not the live table —
// the row is usually already deleted by the time these run. Seeding does
// NOT touch the live table; that is the caller's choice: a stale edit
// must not resurrect it, a resurrection must.
function seedOp(title: string, id: string, at: string) {
  psql(
    `INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
      ` VALUES ('notes', '${id}', jsonb_build_object('id', '${id}', 'title', '${title}', 'body', '', 'updated_at', '${at}'), '${at}');`,
  );
}

function rowIdByTitle(title: string): string {
  // The live row may already be deleted (that is the point of these
  // tests) — the create op's payload in sync_log outlives it.
  return execSync(
    `podman compose exec -T postgres psql -U sync -d offline_notes -tAc ` +
      `"SELECT row_id FROM oxylite.sync_log WHERE payload->>'title' = '${title}' LIMIT 1"`,
    { cwd: "..", encoding: "utf8" },
  ).trim();
}

test("a deleted note never reaches a fresh client", async ({ page, browser }) => {
  await boot(page);
  await addNote(page, DELETED_NOTE);
  await deleteNote(page, DELETED_NOTE);

  // Fresh context = fresh PGlite: whether it bootstraps from a snapshot
  // (deleted rows excluded, tombstones shipped) or replays the ops (the
  // delete is the row's last op), the note must never appear.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: "seed: welcome" })).toBeVisible({
    timeout: 30_000,
  });
  await expect(other.getByRole("listitem").filter({ hasText: DELETED_NOTE })).toHaveCount(0, {
    timeout: 10_000,
  });
  await ctx2.close();
});

test("an offline delete propagates on reconnect", async ({ page, browser }) => {
  await boot(page);
  await addNote(page, DELETED_NOTE);

  // A second, independent client sees the note while the writer is
  // connected.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: DELETED_NOTE })).toBeVisible({
    timeout: 15_000,
  });

  // Offline delete: the row is removed locally and the delete op queues
  // behind the dead socket. The remote copy is untouched meanwhile.
  await page.routeWebSocket("**/sync", (ws) => ws.close());
  await page.reload();
  await expect(page.locator("p").filter({ hasText: /will retry/ })).toBeVisible({
    timeout: 30_000,
  });
  await deleteNote(page, DELETED_NOTE);
  await expect(other.getByRole("listitem").filter({ hasText: DELETED_NOTE })).toBeVisible();

  // Reconnect flushes the queued delete; the other client must drop the
  // row — deletes are writes, delivered by the same machinery.
  await page.routeWebSocket("**/sync", (ws) => ws.connectToServer());
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
  await expect(other.getByRole("listitem").filter({ hasText: DELETED_NOTE })).toHaveCount(0, {
    timeout: 15_000,
  });
  await ctx2.close();
});

test("a cold replay collapses create, delete, and a stale edit without resurrecting", async ({
  browser,
}) => {
  // The batch collapse must replay the merge contract, not "last op in
  // log order": create → delete → stale edit arriving in ONE pull window
  // (a cold client's first pull) must land deleted-with-tombstone. The
  // stale edit is later in the log but older in time — the collapse used
  // to keep it and discard the delete's tombstone, resurrecting the row.
  // Fixed timestamps keep this deterministic (no Date.now() dependence).
  const title = `e2e cold collapse ${Date.now()}`;
  const id = crypto.randomUUID();
  const CREATE_AT = "2026-01-01T00:00:01.000Z";
  const DELETE_AT = "2026-01-01T00:00:02.000Z";
  const STALE_AT = "2026-01-01T00:00:00.000Z";
  psql(
    `INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at) VALUES` +
      ` ('notes', '${id}', jsonb_build_object('id', '${id}', 'title', '${title}', 'body', '', 'updated_at', '${CREATE_AT}'), '${CREATE_AT}'),` +
      // Deletes log the JSON null VALUE (the marker), not SQL NULL.
      ` ('notes', '${id}', 'null'::jsonb, '${DELETE_AT}'),` +
      ` ('notes', '${id}', jsonb_build_object('id', '${id}', 'title', '${title}', 'body', '', 'updated_at', '${STALE_AT}'), '${STALE_AT}');`,
  );

  // One cold client, one pull window covering all three ops.
  const ctx = await browser.newContext();
  const cold = await ctx.newPage();
  await boot(cold);
  await expect(cold.getByRole("listitem").filter({ hasText: "seed: welcome" })).toBeVisible({
    timeout: 30_000,
  });
  await expect(cold.getByRole("listitem").filter({ hasText: title })).toHaveCount(0, {
    timeout: 10_000,
  });
  await ctx.close();
});

test("a stale edit cannot resurrect a deleted row", async ({ page }) => {
  await boot(page);
  await addNote(page, DELETED_NOTE);
  await deleteNote(page, DELETED_NOTE);

  // A client that was offline before the delete now pushes its edit: the
  // op is a valid upsert (not a poison row), but its timestamp loses to
  // the tombstone. The server only streams it (sync_log is the wire
  // history); the guard on the receiving side must drop it silently —
  // no resurrection, and no "Sync failed" notice either.
  const id = rowIdByTitle(DELETED_NOTE);
  seedOp(DELETED_NOTE, id, PAST);

  await expect(page.getByRole("listitem").filter({ hasText: DELETED_NOTE })).toHaveCount(0, {
    timeout: 10_000,
  });
  await expect(page.getByText("Sync failed")).toHaveCount(0);
});

test("a strictly newer edit resurrects the row and clears the tombstone", async ({ page, browser }) => {
  await boot(page);
  await addNote(page, DELETED_NOTE);
  await deleteNote(page, DELETED_NOTE);

  // Deletes can lose like any write: an edit newer than the delete wins
  // (server live table + log, so snapshots stay consistent), the row
  // comes back, and later stale replays lose to the row again.
  const id = rowIdByTitle(DELETED_NOTE);
  const RESURRECTED = `${DELETED_NOTE} resurrected`;
  psql(
    `INSERT INTO notes (id, title, body, updated_at)` +
      ` VALUES ('${id}', '${RESURRECTED}', '', '${FUTURE}');`,
  );
  seedOp(RESURRECTED, id, FUTURE);

  await expect(page.getByRole("listitem").filter({ hasText: RESURRECTED })).toBeVisible({
    timeout: 15_000,
  });

  // A fresh client must see the resurrected row — from the snapshot's
  // live rows, not just from the op replay.
  const ctx2 = await browser.newContext();
  const other = await ctx2.newPage();
  await boot(other);
  await expect(other.getByRole("listitem").filter({ hasText: "seed: welcome" })).toBeVisible({
    timeout: 30_000,
  });
  await expect(other.getByRole("listitem").filter({ hasText: RESURRECTED })).toBeVisible({
    timeout: 15_000,
  });
  await ctx2.close();
});

test("the tombstone guard survives a reload", async ({ page }) => {
  await boot(page);
  await addNote(page, DELETED_NOTE);
  await deleteNote(page, DELETED_NOTE);

  // The engine (and its in-memory state) is destroyed by the reload; the
  // tombstone lives in PGlite. A stale edit arriving AFTER the reload —
  // streamed to the fresh engine — must still lose.
  await page.reload();
  await expect(page.getByRole("listitem").filter({ hasText: "seed: welcome" })).toBeVisible({
    timeout: 30_000,
  });

  const id = rowIdByTitle(DELETED_NOTE);
  seedOp(DELETED_NOTE, id, PAST);

  await expect(page.getByRole("listitem").filter({ hasText: DELETED_NOTE })).toHaveCount(0, {
    timeout: 10_000,
  });
  await expect(page.getByText("Sync failed")).toHaveCount(0);
});
