import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

// Conflict + durability contracts under concurrency and slow arrival.
// The server-side Postgres is shared across parallel workers, so every
// stamp is unique per run; absence assertions ride a control canary that
// must stay visible throughout (an absent note only means something on a
// page that is demonstrably live and synced).

function psql(sql: string) {
  execSync(`${process.env.COMPOSE ?? "podman compose"} exec -T postgres psql -U sync -d offline_notes -c "${sql}"`, {
    cwd: "..", // playwright runs from e2e/; compose file lives at the repo root
  });
}

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
}

async function deleteNote(page: Page, title: string) {
  await page.getByRole("listitem").filter({ hasText: title }).getByRole("button").click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toHaveCount(0);
}

async function waitConnected(page: Page) {
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

async function goOffline(page: Page) {
  // Force-close every sync socket (Chrome keeps established sockets open
  // in offline emulation, so this simulates a dead connection instead),
  // then reload so the app establishes a fresh (closed) one.
  await page.routeWebSocket("**/sync", (ws) => ws.close());
  await page.reload();
  await expect(page.locator("p").filter({ hasText: /offline/ })).toBeVisible({
    timeout: 30_000,
  });
}

async function goOnline(page: Page) {
  // Re-route with a pass-through handler (the newer registration wins)
  // and wait for the reconnect (3s backoff).
  await page.routeWebSocket("**/sync", (ws) => ws.connectToServer());
  await waitConnected(page);
}

test("a push accepted by a socket the server never saw must still sync", async ({ page }) => {
  // The durability contract behind ack-based op-log retention: the op log
  // row may only be deleted once the SERVER confirmed the write, not once
  // the socket accepted the frame. A socket can accept a send and die
  // before the server reads it (proxy, crash) — the next real reconnect
  // must resend. Interposed here as a silent mock: the client sees an
  // open socket ("connected"), flushes the pending op into it, and the
  // op goes nowhere.
  await page.goto("/");
  await waitConnected(page);

  const title = `e2e ack canary ${Date.now()}`;
  await goOffline(page);
  await addNote(page, title); // queued in pending_ops

  // Accept the reconnect but never forward anything: Pull and Push are
  // silently discarded. flush_pending sees OPEN, send_ok succeeds, and
  // the pending row is deleted — the bug under review.
  await page.routeWebSocket("**/sync", () => {});
  await waitConnected(page);
  // The session loop drains within one 150ms tick of OPEN; wait past it
  // so the flush has definitely run before the real server returns.
  await page.waitForTimeout(750);

  await goOnline(page);
  await page.reload();
  await waitConnected(page);

  // A fresh client must eventually receive the note: today the pending
  // row is already gone, nothing is resent, and this times out — the
  // failure this review exists to fix.
  const ctxB = await page.context().browser()!.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await expect(pageB.getByRole("listitem").filter({ hasText: title })).toBeVisible({
    timeout: 30_000,
  });
  await ctxB.close();
});

test("a lost ack resends the batch — the server reapplies it and every client converges", async ({
  page,
}) => {
  // The Ack is itself lossy: the server can apply a push, log it, and
  // have the socket die before the Ack lands. The pending row then
  // survives and the next connect RESENDS it — the server must absorb
  // the duplicates (guarded upsert is a no-op against the identical
  // row; the log is the wire history) while every client converges to
  // exactly one row.
  //
  // At-least-once bounds NOTHING about the arrival count: each unacked
  // batch resends on every connect, and the transport has been observed
  // to double a push outright (the intermittent duplicate in
  // impl/client-drain.md — three arrivals on CI). The PINNED property
  // is "the op was not lost" (≥2 arrivals) + exact convergence (one
  // live row, one UI row, a fresh client agrees).
  //
  // Deterministic by construction: registering onMessage on the
  // SERVER-side route stops Playwright's auto-forwarding in that
  // direction, so the page→server Push is relayed untouched (applied,
  // logged) while every Ack is swallowed — the client never retires
  // the batch. A reload then forces the reconnect that resends it.
  const stamp = `e2e lost-ack ${Date.now()}`;

  await page.goto("/");
  await waitConnected(page);

  await page.routeWebSocket("**/sync", (ws) => {
    const server = ws.connectToServer();
    server.onMessage((msg) => {
      if (typeof msg === "string" && msg.includes('"type":"Ack"')) {
        return; // lost in flight
      }
      ws.send(msg);
    });
  });
  await page.reload();
  await waitConnected(page);

  await addNote(page, stamp); // applied + logged server-side, ack swallowed
  // The server has committed the push long before the reconnect below
  // (3s backoff + boot); this bound just sequences the steps.
  await page.waitForTimeout(500);

  await page.routeWebSocket("**/sync", (ws) => ws.connectToServer());
  await page.reload();
  await waitConnected(page); // flush resends the unacked batch

  // The log records EVERY arrival (at-least-once, by design — two is
  // the typical shape: the original + one resend, but the count is
  // unbounded by contract) while the live table holds exactly one row
  // — the duplicates were absorbed.
  const logCount = () => {
    try {
      const out = execSync(
        `${process.env.COMPOSE ?? "podman compose"} exec -T postgres psql -U sync -d offline_notes -t -A -c ` +
          `"SELECT count(*) FROM oxylite.sync_log WHERE payload->>'title' = '${stamp}'"`,
        { cwd: "..", stdio: ["ignore", "pipe", "ignore"] },
      )
        .toString()
        .trim();
      return Number(out);
    } catch {
      return 0;
    }
  };
  await expect.poll(logCount, { timeout: 15_000 }).toBeGreaterThanOrEqual(2);

  const rowCount = execSync(
    `${process.env.COMPOSE ?? "podman compose"} exec -T postgres psql -U sync -d offline_notes -t -A -c ` +
      `"SELECT count(*) FROM notes WHERE title = '${stamp}'"`,
    { cwd: "..", stdio: ["ignore", "pipe", "ignore"] },
  )
    .toString()
    .trim();
  expect(rowCount).toBe("1");

  // One row in the UI, and a fresh client replays every log entry into
  // the same single row — converged, not duplicated.
  await expect(page.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(1);
  const ctxB = await page.context().browser()!.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await expect(pageB.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(1, {
    timeout: 30_000,
  });
  await ctxB.close();
});

test("an offline delete loses to an unseen newer edit — the note resurrects everywhere", async ({
  page,
}) => {
  // Pull-before-flush ordering contract: the reconnect must apply the
  // server backlog BEFORE flushing pending ops, so a pending delete that
  // predates an edit the client hasn't seen meets that edit's tombstone
  // guard locally (resurrect) instead of clobbering a row the server
  // considers alive. The seeded edit is future-stamped to make "newer"
  // deterministic regardless of client clocks.
  const stamp = `e2e resurrect ${Date.now()}`;
  await page.goto("/");
  await waitConnected(page);
  await addNote(page, stamp);
  await goOffline(page);

  const future = `now() + interval '1 hour'`;
  psql(
    `WITH u AS (` +
      ` UPDATE notes SET title = '${stamp} edited', updated_at = ${future}` +
      ` WHERE title = '${stamp}'` +
      ` RETURNING id, title, body, updated_at` +
      `) INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
      ` SELECT 'notes', id,` +
      ` jsonb_build_object('id', id, 'title', title, 'body', body, 'updated_at', updated_at),` +
      ` updated_at FROM u;`,
  );

  await deleteNote(page, stamp); // wins locally (local row is older), queued
  await goOnline(page);

  // The unseen edit (applied by the pre-flush pull) is strictly newer
  // than the pending delete: the note must come back with the edited
  // title, and the late delete must lose on the server too.
  await expect(
    page.getByRole("listitem").filter({ hasText: `${stamp} edited` }),
  ).toBeVisible({ timeout: 30_000 });

  const ctxB = await page.context().browser()!.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await expect(pageB.getByRole("listitem").filter({ hasText: `${stamp} edited` })).toBeVisible({
    timeout: 30_000,
  });
  await ctxB.close();

  // Cleanup: the +1h seed makes this row undeletable by real-time
  // deletes for an hour — a manual Remove loses the LWW check (by
  // design). Out-stamp it (+2h delete op) so the shared dev DB does
  // not accumulate haunted notes; every client drops it on replay.
  psql(
    `WITH gone AS (` +
      ` DELETE FROM notes WHERE title = '${stamp} edited' RETURNING id` +
      `), t AS (` +
      ` INSERT INTO oxylite.tombstones (table_name, id, deleted_at)` +
      ` SELECT 'notes', id, (now() + interval '2 hours')::timestamptz FROM gone` +
      `) INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
      ` SELECT 'notes', id, 'null'::jsonb, (now() + interval '2 hours')::timestamptz FROM gone;`,
  );
});

test("concurrent offline deletes of the same note converge — deleted everywhere", async ({
  browser,
}) => {
  const stamp = `e2e conc-del ${Date.now()}`;
  const canary = `e2e conc-del control ${Date.now()}`;

  const ctxA = await browser.newContext();
  const pageA = await ctxA.newPage();
  await pageA.goto("/");
  await waitConnected(pageA);
  await addNote(pageA, stamp);
  await addNote(pageA, canary);

  // Client B must have the row before both go offline (same local state,
  // then true concurrency: two independent op logs, two sockets).
  const ctxB = await browser.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await expect(pageB.getByRole("listitem").filter({ hasText: stamp })).toBeVisible({
    timeout: 30_000,
  });
  await expect(pageB.getByRole("listitem").filter({ hasText: canary })).toBeVisible();
  await expect(pageA.getByRole("listitem").filter({ hasText: canary })).toBeVisible();

  await goOffline(pageA);
  await goOffline(pageB);
  await deleteNote(pageA, stamp);
  await deleteNote(pageB, stamp);
  await goOnline(pageA);
  await goOnline(pageB);

  // Both deletes converge on the same tombstone: the note stays gone on
  // every client, no resurrection, no phantom row. The control canary
  // proves both pages are live and synced, so the absence is meaningful.
  await expect(pageA.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(0);
  await expect(pageB.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(0);
  await expect(pageA.getByRole("listitem").filter({ hasText: canary })).toBeVisible();
  await expect(pageB.getByRole("listitem").filter({ hasText: canary })).toBeVisible();

  const ctxC = await browser.newContext();
  const pageC = await ctxC.newPage();
  await pageC.goto("/");
  await expect(pageC.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(0, {
    timeout: 30_000,
  });
  await expect(pageC.getByRole("listitem").filter({ hasText: canary })).toBeVisible();
  await ctxC.close();
  await ctxA.close();
  await ctxB.close();
});

test("a reload during a snapshot backlog loses no row (cursor is saved after apply)", async ({
  browser,
}) => {
  // The snapshot path persists the cursor only AFTER the batch applied
  // (engine applies rows, then save_cursor) — a reload mid-apply re-
  // snapshots on the next boot and converges. This pins that order: if
  // the cursor ever moves before the snapshot lands, rows go missing
  // permanently and this test fails.
  const stamp = `e2e snapreload ${Date.now()}`;
  psql(
    `INSERT INTO notes (id, title, body, updated_at)` +
      ` SELECT gen_random_uuid(), '${stamp} ' || g, '', now() - (g || ' seconds')::interval` +
      ` FROM generate_series(1, 120) g;` +
      ` INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
      ` SELECT 'notes', id,` +
      ` jsonb_build_object('id', id, 'title', title, 'body', body, 'updated_at', updated_at),` +
      ` updated_at FROM notes WHERE title LIKE '${stamp}%';`,
  );

  const ctx = await browser.newContext();
  const cold = await ctx.newPage();
  await cold.goto("/");
  await waitConnected(cold);
  // Reload immediately: the snapshot apply of 120 rows is still in
  // flight (or about to start). Either way the end state must be every
  // row — the assertion holds whichever side of the apply we land on.
  await cold.reload();

  const seeded = cold.getByRole("listitem").filter({ hasText: stamp });
  await expect(seeded).toHaveCount(120, { timeout: 60_000 });
  await ctx.close();
});

test("flush + own echo are idempotent — one reconnect, one row, ever", async ({ page }) => {
  // At-least-once means the client sees its own op again after the
  // flush (Ack triggers a pull; the ticker may echo it too). LWW makes
  // re-application a no-op — pin that: exactly one row, stable across
  // repeated reconnects.
  const stamp = `e2e echo ${Date.now()}`;
  const canary = `e2e echo control ${Date.now()}`;
  await page.goto("/");
  await waitConnected(page);
  await addNote(page, canary);
  await goOffline(page);
  await addNote(page, stamp); // queued
  await goOnline(page); // flush → Ack → pull → own echo re-applied

  await expect(page.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(1);
  await expect(page.getByRole("listitem").filter({ hasText: canary })).toBeVisible();

  // A second full cycle must not duplicate anything.
  await goOffline(page);
  await goOnline(page);
  await expect(page.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(1);

  const ctxB = await page.context().browser()!.newContext();
  const pageB = await ctxB.newPage();
  await pageB.goto("/");
  await expect(pageB.getByRole("listitem").filter({ hasText: stamp })).toHaveCount(1, {
    timeout: 30_000,
  });
  await ctxB.close();
});
