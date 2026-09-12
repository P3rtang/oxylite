import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

// ROADMAP 2.2's end-to-end contract, measured where the lib owns it: a
// COMMITTED push reaches a connected client through the notify path
// (trigger → adapter → bus → wake → pull), not the poll ticker.
//
// The metric is COMMIT → RECEIPT, built entirely from existing surfaces
// (reviewer caution on new logging: none is added — the flag condition
// is moot):
//   - the commit instant is `oxylite.sync_log.logged_at` (server
//     arrival, 0007), fetched in ONE psql call after the writes;
//   - the receipt instants are client B's own "[sync] received N
//     events, cursor -> X" console lines, captured with runner
//     timestamps (same host clock — podman shares the kernel clock).
// A row's delivery = the first receipt whose cursor covers its seq and
// whose timestamp postdates the commit. Under the old 500ms ticker that
// first post-commit pull IS a tick, so the tick wait sits inside the
// delta — ten-for-ten ≤350ms is ≈0.1% likely on the ticker, green on
// notify (wire ~30–80ms + B's 150ms drain cap).
//
// The DOM render is deliberately NOT the assertion: the client
// machinery (persist round trip, inbox drain, reactive re-render ≈
// 750–860ms observed) is the ruled follow-up's contract, and its noise
// swamps the server-side win.
//
// A dedicated section per reviewer ruling: clear of the older specs'
// queue/seq seeds, still at max workers. Stamps are unique per run;
// parallel workers' rows legitimately reach this client too — the
// cursor-covering rule handles that (a foreign-triggered pull that
// carries our row is still an honest upper bound of its delivery).

const WRITES = 10;
const DELTA_BUDGET_MS = 350;
const SANITY_BUDGET_MS = 5_000;
const CLOCK_SLACK_MS = 10;

function psql(sql: string) {
  return execSync(
    `podman compose exec -T postgres psql -U sync -d offline_notes -t -A -c "${sql}"`,
    { cwd: "..", encoding: "utf8" }, // playwright runs from e2e/; compose file at repo root
  );
}

async function waitConnected(page: Page) {
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

test("a committed push is delivered to a connected client within the notify budget", async ({
  browser,
}) => {
  const run = `e2e latency ${Date.now()}`;
  const stamps = Array.from({ length: WRITES }, (_, i) => `${run} #${i}`);

  // Client B: the idle reader, connected and current before any timed
  // write. Its console stream is the receipt source.
  const ctxB = await browser.newContext();
  const pageB = await ctxB.newPage();
  const receipts: { ts: number; cursor: number }[] = [];
  pageB.on("console", (m) => {
    const hit = m.text().match(/received (\d+) events, cursor -> (\d+)/);
    if (hit && Number(hit[1]) > 0) {
      receipts.push({ ts: Date.now(), cursor: Number(hit[2]) });
    }
  });
  await pageB.goto("/");
  await waitConnected(pageB);

  // Client A: the writer.
  const ctxA = await browser.newContext();
  const pageA = await ctxA.newPage();
  await pageA.goto("/");
  await waitConnected(pageA);

  for (const stamp of stamps) {
    await pageA.getByPlaceholder("Note title…").fill(stamp);
    await pageA.getByRole("button", { name: "Add" }).click();
    // Sanity, not the contract: the row must arrive at B (loose), the
    // TIMED assertion is the commit→receipt delta below.
    await expect(pageB.getByRole("listitem").filter({ hasText: stamp })).toBeVisible({
      timeout: SANITY_BUDGET_MS,
    });
  }

  // One round trip: commit instant + seq per stamp (unique-stamp match
  // keeps parallel workers' rows out).
  const rows = psql(
    `SELECT payload->>'title', seq, (EXTRACT(EPOCH FROM logged_at)*1000)::bigint` +
      ` FROM oxylite.sync_log WHERE payload->>'title' LIKE '${run} #%' ORDER BY seq;`,
  )
    .trim()
    .split("\n")
    .map((line) => {
      const [title, seq, committedMs] = line.split("|");
      return { title, seq: Number(seq), committedMs: Number(committedMs) };
    });
  expect(rows).toHaveLength(WRITES);

  const deltas = rows.map(({ title, seq, committedMs }) => {
    const delivery = receipts.find(
      (r) => r.cursor >= seq && r.ts >= committedMs - CLOCK_SLACK_MS,
    );
    expect(delivery, `no receipt covering seq ${seq} (${title})`).toBeDefined();
    return { title, delta: delivery!.ts - committedMs };
  });

  for (const { title, delta } of deltas) {
    console.log(`[push-latency] ${title}: commit→receipt ${delta}ms`);
    expect(delta, `${title} delivered late`).toBeLessThanOrEqual(DELTA_BUDGET_MS);
  }

  await ctxA.close();
  await ctxB.close();
});
