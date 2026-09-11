import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

// Apply failures are data (see spec/errors.md): a row the sink can't apply
// must surface in the notice overlay, never vanish into the console.

test("apply failures surface in the notice overlay", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("connected")).toBeVisible({ timeout: 30_000 });

  // A malformed sync_log row (payload without the required fields) must
  // fail the notes sink and surface as a notice — not vanish into the
  // console. Inserted AFTER connect, so the ticker streams it directly
  // (no snapshot rebuild can swallow it). The ENVELOPE timestamp stays
  // canonical: the server's push door validates it (a malformed envelope
  // would fail the stream itself and stall every client — typed, by
  // design); the payload is what must poison the apply.
  execSync(
    `podman compose exec -T postgres psql -U sync -d offline_notes -c ` +
      `"INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at) ` +
      `VALUES ('notes', gen_random_uuid(), '{}'::jsonb, '2026-01-01T00:00:00.000Z');"`,
    { cwd: ".." }, // playwright runs from e2e/; compose file is at the repo root
  );

  // Distinct skip counts produce distinct toasts — several may stack.
  // Assert on the first one: it must appear and then auto-dismiss (4s).
  const toast = page.getByText("Sync failed").first();
  await expect(toast).toBeVisible({ timeout: 10_000 });
  await expect(toast).toBeHidden({ timeout: 12_000 });
});
