/**
 * PGlite bootstrap, called from Rust (src/pglite.rs) via `eval` with the
 * shared migration list as its argument: [{ name, up }, ...].
 *
 * Embedded into the wasm at compile time via include_str! — not served —
 * so this function and the Rust bridge evolve together. It is eval'd as an
 * expression and immediately invoked with the migration array.
 *
 * Constructions are deduped by caching the in-flight promise on
 * globalThis.__pgliteReady: two Rust tasks racing init before either has a
 * resolved instance must share ONE construction instead of opening two
 * emscripten modules on the same IndexedDB dir. (A pending promise can't
 * be cached Rust-side without extra plumbing, so the in-flight guard lives
 * here; the resolved instance is stored Rust-side, in the singleton.)
 *
 * PGlite 0.5.x has no built-in migration runner, so this applies the
 * migrations itself: applied names are tracked in the client's own `meta`
 * table, mirroring sqlx's `_sqlx_migrations`.
 */
async (schema, migrations) => {
    if (!globalThis.__pgliteReady) {
        globalThis.__pgliteReady = (async () => {
            const base = new URL("/pglite/index.js", location.href);
            const m = await import(base);
            const db = await new m.PGlite({
                // Default is an in-memory FS; idb:// selects the
                // IndexedDB-backed IdbFs so data survives reloads without
                // the server.
                dataDir: "idb://offline_notes",
            });
            // Bootstrap the migration tracker before anything else, then
            // apply pending migrations apply-once like sqlx does
            // server-side. Idempotent DDL keeps a half-applied set healable.
            // The lib's namespace exists before anything of ours does;
            // the migration tracker lives inside it (like _sqlx_migrations).
            await db.exec("CREATE SCHEMA IF NOT EXISTS " + schema);
            await db.exec(
                `CREATE TABLE IF NOT EXISTS ${schema}.meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)`,
            );
            for (const m of migrations) {
                const key = "migration:" + m.name;
                const seen = await db.query(
                    `SELECT 1 FROM ${schema}.meta WHERE key = $1`, [key]);
                if (seen.rows.length === 0) {
                    await db.exec(m.up);
                    await db.query(
                        `INSERT INTO ${schema}.meta (key, value) VALUES ($1, '1')`,
                        [key],
                    );
                }
            }
            // The resolved instance is stored Rust-side (the thread_local
            // singleton); this promise only exists to dedupe concurrent
            // in-flight constructions.
            return db;
        })();
        // Drop a failed init so the next call retries cleanly.
        globalThis.__pgliteReady.catch(() => {
            globalThis.__pgliteReady = null;
        });
    }
    return globalThis.__pgliteReady;
}
