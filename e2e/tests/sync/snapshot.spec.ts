import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

// Snapshot-apply robustness: a snapshot whose rows fail to apply locally
// must fail LOUDLY (LAST_ERROR → the notice overlay) and RECOVER (the
// cursor must not advance past data the client never wrote — the same
// contract the Events path got in #33). Today the snapshot arm only
// console-logs the failure while the cursor jumps to the snapshot seq —
// the reconnect then pulls from above every real row, and everything the
// snapshot carried strands silently.
//
// The poison is crafted client-bound over a mocked socket because the
// real server CANNOT produce it: uuid/timestamptz/jsonb all validate at
// the storage layer. That is the point — the client is the compat
// boundary and must not silently diverge on garbage from the wire. The
// trigger is serde-legal but SQL-fatal: a title carrying a NUL byte
// (Postgres rejects 0x00 in text), which fails the whole upsert chunk —
// probed against the vendored PGlite: `invalid byte sequence for
// encoding "UTF8": 0x00`.

function psql(sql: string) {
  execSync(`podman compose exec -T postgres psql -U sync -d offline_notes -c "${sql}"`, {
    cwd: "..", // playwright runs from e2e/; compose file lives at the repo root
  });
}

async function waitConnected(page: Page) {
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

test("a snapshot that fails to apply surfaces the error and recovers on reconnect", async ({
  page,
}) => {
  const stamp = `e2e snappoison ${Date.now()}`;
  const keeper = `${stamp} keeper`;
  const control = `${stamp} control`;
  const poisonTitle = `${stamp} poisoned`;

  // Both rows ride the REAL log so only a genuine re-pull can surface
  // them — the crafted snapshot deliberately carries no recoverable
  // copy, so a fix that "recovers" by applying crafted data cannot pass.
  psql(
    `INSERT INTO notes (id, title, body, updated_at)` +
      ` SELECT gen_random_uuid(), t, '', now() FROM (VALUES ('${keeper}'), ('${control}')) v(t);` +
      ` INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)` +
      ` SELECT 'notes', id, jsonb_build_object('id', id, 'title', title, 'body', body, 'updated_at', updated_at), updated_at` +
      ` FROM notes WHERE title IN ('${keeper}', '${control}');`,
  );

  const consoleLines: string[] = [];
  page.on("console", (m) => consoleLines.push(m.text()));

  // Connection 1 is the poison session: handshake echo + one crafted
  // Snapshot, then SILENCE — no server push will ever arrive on this
  // socket, so recovery must be client-initiated. Later connections are
  // pass-through: the reconnect re-pulls from the real server.
  let conns = 0;
  await page.routeWebSocket("**/sync", (ws) => {
    conns += 1;
    if (conns > 1) {
      ws.connectToServer();
      return;
    }
    ws.onMessage((msg) => {
      if (typeof msg !== "string") return;
      const parsed = JSON.parse(msg);
      if (parsed.type === "Hello") {
        // Echo the client's own version — always wire-compatible.
        ws.send(JSON.stringify({ type: "Ready", version: parsed.version }));
      } else if (parsed.type === "Pull") {
        // seq far above any real head: the bug path strands the cursor
        // above every row that exists.
        ws.send(
          JSON.stringify({
            type: "Snapshot",
            seq: 999_999,
            tables: [
              {
                table: "notes",
                rows: [
                  {
                    id: crypto.randomUUID(),
                    title: `${poisonTitle}\u0000`,
                    body: "",
                    updated_at: new Date().toISOString(),
                  },
                ],
              },
            ],
            tombstones: [],
          }),
        );
      }
    });
  });

  await page.goto("/");
  await waitConnected(page);

  // The crafted snapshot landed — the test cannot pass vacuously.
  await expect
    .poll(() => consoleLines.join("\n"), { timeout: 15_000 })
    .toContain("snapshot at");

  // The failure must be surfaced (LAST_ERROR → notice overlay), not
  // console-only. RED today: the snapshot arm only logs.
  await expect(page.getByText("Sync failed")).toBeVisible({ timeout: 8_000 });

  // And the client must recover: cursor stays put, the reconnect
  // re-pulls, the real rows land. RED today: the cursor jumped to the
  // snapshot seq, so the reconnect's pull returns nothing and these
  // rows strand forever.
  await expect(page.getByRole("listitem").filter({ hasText: keeper })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByRole("listitem").filter({ hasText: control })).toBeVisible();

  // The poison row itself never becomes visible (its chunk failed) —
  // the absence is meaningful with keeper + control demonstrably live.
  await expect(page.getByRole("listitem").filter({ hasText: poisonTitle })).toHaveCount(0);
});
