import { expect, test, type Page } from "@playwright/test";

const NOTE_A = `e2e note alpha ${Date.now()}`;

async function addNote(page: Page, title: string) {
  await page.getByPlaceholder("Note title…").fill(title);
  await page.getByRole("button", { name: "Add" }).click();
  await expect(page.getByRole("listitem").filter({ hasText: title })).toBeVisible();
}

// Pure app behavior: local persistence via PGlite in IndexedDB, no sync.

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
