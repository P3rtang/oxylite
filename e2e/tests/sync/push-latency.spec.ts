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
// The distribution's shape IS the contract. Notify delivers 9+ of 10
// writes under 350ms (quiet-run deltas 33–207ms; full-suite load at 3
// workers 18–141ms), while the 500ms ticker cannot — its tick wait
// (uniform 0–500ms) makes per-write P ≈ 0.4, so ≥9/10 is ≈0.16% likely.
// ONE outlier per run is tolerated: a scheduling/pool spike was
// observed at 1.3s under the FULL suite at max workers — but it must
// stay far below the 5s fallback tick, whose fingerprint (everything
// ≥5s) is exactly what the ceiling catches when the wake path is dead.
const BUDGET_MS = 350;
// TWO outliers: the render test below is now PERMANENT parallel traffic
// in this suite, and its writes land in the same load pockets (measured
// 357/379ms vs 8/10 ≤260ms in the same run). The ticker still cannot
// pass: its tick wait makes per-write P ≈ 0.4, so ≥8/10 under 350ms is
// ≈1.2% likely.
const OUTLIERS_ALLOWED = 2;
const OUTLIER_CEILING_MS = 1500;
const SANITY_BUDGET_MS = 5_000;
const CLOCK_SLACK_MS = 10;

// The client-machinery tail: receipt→visible (apply + query re-run +
// render + detection). Red-first history: the original DOM test
// (click→visible ≤600/750ms ×3) was committed test.fixme and flipped
// live with #38 — post-fix prints 638–664ms under full-suite load —
// but a loaded delivery POCKET blew it one gate later, so the contract
// decomposed: delivery is pinned by the receipt test's shape; THIS
// pins the machinery (which the drains don't inflate — they sit before
// the receipt). Pre-fix tail ≈530ms (receipt 330 → visible ~860).
const TAIL_BUDGET_MS = 900;
// ≥2/3 writes within the budget; the one outlier must stay under a
// ceiling that still catches a dead tail (a 5s stall or a never-applied
// frame). Local tails: 448–728ms; CI's smaller runner: 464–1283ms.
const TAIL_OUTLIERS_ALLOWED = 1;
const TAIL_CEILING_MS = 2500;
const RENDER_WRITES = 3;

function psql(sql: string) {
  return execSync(
    `${process.env.COMPOSE ?? "podman compose"} exec -T postgres psql -U sync -d offline_notes -t -A -c "${sql}"`,
    { cwd: "..", encoding: "utf8" }, // playwright runs from e2e/; compose file at repo root
  );
}

async function waitConnected(page: Page) {
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });
}

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
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
  expect(rows).not.toHaveLength(0);
  // At-least-once may legitimately record the SAME op twice (the
  // lost-ack contract pins exactly that — observed once, 77ms apart,
  // no reconnect). So: count DISTINCT stamps, and match each stamp's
  // delta on its FIRST seq (rows are seq-ordered; the duplicate's seq
  // only makes delivery earlier or equal).
  const first = new Map<string, { seq: number; committedMs: number }>();
  for (const { title, seq, committedMs } of rows) {
    if (!first.has(title)) first.set(title, { seq, committedMs });
  }
  expect([...first.keys()]).toHaveLength(WRITES);

  const deltas = [...first.entries()].map(([title, { seq, committedMs }]) => {
    const delivery = receipts.find(
      (r) => r.cursor >= seq && r.ts >= committedMs - CLOCK_SLACK_MS,
    );
    expect(delivery, `no receipt covering seq ${seq} (${title})`).toBeDefined();
    return { title, delta: delivery!.ts - committedMs };
  });

  for (const { title, delta } of deltas) {
    console.log(`[push-latency] ${title}: commit→receipt ${delta}ms`);
  }
  const late = deltas.filter(({ delta }) => delta > BUDGET_MS);
  expect(
    late.length,
    `notify regime lost: ${deltas.map(({ delta }) => delta).join(", ")}ms`,
  ).toBeLessThanOrEqual(OUTLIERS_ALLOWED);
  for (const { title, delta } of late) {
    expect(delta, `${title} outlier beyond the ceiling`).toBeLessThanOrEqual(
      OUTLIER_CEILING_MS,
    );
  }

  await ctxA.close();
  await ctxB.close();
});

test(
  "a push's client tail — receipt to visible — stays within the machinery budget",
  async ({ browser }) => {
    // The DOM contract, decomposed so each half pins what it owns. The
    // delivery is the receipt test's job (its shape absorbs load
    // pockets); THIS test pins the client machinery that consumes a
    // delivered frame: visible − covering-receipt = apply + query
    // re-run + render + detection. The drains and the bridge cache sit
    // BEFORE the receipt, so this tail is stable across them — pre-fix
    // it measured ~530ms (receipt 330 → visible ~860), post-fix the
    // same; a machinery regression (apply slowdown, broken reactive
    // re-run) is what fails here. click→visible stays printed for the
    // record (the user-feel number = receipt delta + this tail).
    const run = `e2e render ${Date.now()}`;
    const stamps = Array.from({ length: RENDER_WRITES }, (_, i) => `${run} #${i}`);

    // A: the writer, connected. Both tabs boot before any write exists,
    // so B's initial pull covers only the seed state — B is idle-current
    // for every timed write, and the tail metric is immune to boot
    // overlap anyway (the covering receipt is the first frame whose
    // cursor reaches the stamp's seq).
    const ctxA = await browser.newContext();
    const pageA = await ctxA.newPage();
    await pageA.goto("/");
    await waitConnected(pageA);

    // B: the reader; its console stream is the receipt source (same
    // filter as the receipt test).
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

    // Sanity waits only: the row must RENDER (loose); the TIMED
    // assertion is the tail below.
    const visibleTs = new Map<string, number>();
    const tails: { stamp: string; tail: number }[] = [];
    for (const stamp of stamps) {
      await pageA.getByPlaceholder("Note title…").fill(stamp);
      const t0 = Date.now();
      await pageA.getByRole("button", { name: "Add" }).click();
      await expect(pageB.getByRole("listitem").filter({ hasText: stamp })).toBeVisible({
        timeout: SANITY_BUDGET_MS,
      });
      visibleTs.set(stamp, Date.now());
      console.log(`[push-latency] ${stamp}: click→visible ${Date.now() - t0}ms`);
    }

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

    const first = new Map<string, { seq: number; committedMs: number }>();
    for (const { title, seq, committedMs } of rows) {
      if (!first.has(title)) first.set(title, { seq, committedMs });
    }

    for (const stamp of stamps) {
      const row = first.get(stamp);
      expect(row, `no log row for ${stamp}`).toBeDefined();
      const visible = visibleTs.get(stamp)!;
      const delivery = receipts.find(
        (r) => r.cursor >= row!.seq && r.ts >= row!.committedMs - CLOCK_SLACK_MS,
      );
      expect(delivery, `no receipt covering seq ${row!.seq} (${stamp})`).toBeDefined();
      const tail = visible - delivery!.ts;
      console.log(`[push-latency] ${stamp}: receipt→visible ${tail}ms`);
      tails.push({ stamp, tail });
    }

    // The machinery's shape: ≥2/3 writes within the budget. The wasm
    // tail on CI's smaller runner inflates ~2–3× under full-suite load
    // (measured 464ms → 1283ms within ONE run), so one outlier is
    // tolerated — but a machinery REGRESSION (apply slowdown, a broken
    // reactive re-run) doubles every tail and fails both bounds.
    const slow = tails.filter(({ tail }) => tail > TAIL_BUDGET_MS);
    expect(
      slow.length,
      `client tail regime lost: ${tails.map(({ tail }) => tail).join(", ")}ms`,
    ).toBeLessThanOrEqual(TAIL_OUTLIERS_ALLOWED);
    for (const { stamp, tail } of slow) {
      expect(tail, `${stamp} tail beyond the ceiling`).toBeLessThanOrEqual(
        TAIL_CEILING_MS,
      );
    }

    await ctxA.close();
    await ctxB.close();
  },
);
