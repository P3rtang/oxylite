use core::cell::RefCell;
use js_sys::Function;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};

/// Rust bridge to the vendored PGlite (the official ESM bundle copied
/// verbatim into `crates/client/assets/pglite/`). No JS of our own is
/// maintained: we instantiate PGlite's own artifact and call its public
/// `query`/`exec` API through wasm-bindgen.
///
/// PGlite persists its data dir in IndexedDB via its IdbFs
/// (`dataDir: "idb://offline_notes"`), so writes survive page reloads and
/// keep the app functional while the server is unreachable.
#[derive(Clone)]
pub struct Pglite {
    instance: JsValue,
}

// Singleton handle, embedded-book style (peripherals/singletons): an
// Option-typed static that init fills exactly once and everyone else
// borrows a cheap clone of. wasm runs single-threaded here, so a
// thread_local + RefCell is sound WITHOUT the unsafe `static mut` +
// taken-flag dance the embedded version needs on real hardware.
thread_local! {
    static INSTANCE: RefCell<Option<Pglite>> = const { RefCell::new(None) };
}

#[wasm_bindgen]
unsafe extern "C" {
    #[wasm_bindgen(catch, js_name = "eval")]
    async fn eval_js(code: &str) -> Result<JsValue, JsValue>;
}

impl Pglite {
    /// Open (or reopen) the persistent offline database, applying
    /// `migrations` (the shared set, see `shared::MIGRATIONS`) before anyone
    /// can touch it.
    ///
    /// Singleton lifecycle, embedded-book style: once a construction
    /// succeeds, `INSTANCE` holds an owned `Pglite` and every later call
    /// returns a cheap clone — callers can hand `&Pglite` around without
    /// re-initializing. `init` never returns `None`-take semantics (unlike
    /// `Peripherals::take`): the UI calls it from many tasks, so re-joining
    /// the live instance is the point, not a borrow-checker escape hatch.
    ///
    /// `new PGlite(...)` must happen in JS — wasm-bindgen `call` cannot
    /// construct ES classes — so this is driven by an eval'd snippet. The
    /// snippet keeps its in-flight promise on `globalThis.__pgliteReady` so
    /// two Rust tasks racing `init` before either has a resolved instance
    /// share ONE construction instead of opening two emscripten modules on
    /// the same IndexedDB dir. (A pending promise can't be cached in Rust
    /// without extra plumbing, so the in-flight guard lives in JS; the
    /// resolved instance is stored Rust-side, in `INSTANCE`.)
    /// wasm-bindgen async externs auto-await the returned promise, so
    /// `eval_js` resolves to the instance itself.
    ///
    /// PGlite 0.5.x has no built-in migration runner, so the snippet applies
    /// the shared migrations itself: applied names are tracked in the
    /// client's own `meta` table, mirroring sqlx's `_sqlx_migrations`.
    pub async fn init(migrations: &[(&'static str, &'static str)]) -> Result<Pglite, JsValue> {
        if let Some(existing) = INSTANCE.with(|i| i.borrow().clone()) {
            return Ok(existing);
        }

        // The server serves the vendored bundle at /pglite/ with proper MIME
        // types. A dynamic import inside eval() can only resolve absolute
        // URLs, so build the full URL from the document location.
        let migs: Vec<String> = migrations
            .iter()
            .map(|(name, up)| {
                format!(
                    "{{\"name\":{},\"up\":{}}}",
                    serde_json::to_string(name).unwrap(),
                    serde_json::to_string(up).unwrap()
                )
            })
            .collect();
        let migs = migs.join(",");
        let code = format!(
            r#"(async () => {{
                if (!globalThis.__pgliteReady) {{
                    globalThis.__pgliteReady = (async () => {{
                        const base = new URL("/pglite/index.js", location.href);
                        const m = await import(base);
                        const db = await new m.PGlite({{
                            // Default is an in-memory FS; idb:// selects the
                            // IndexedDB-backed IdbFs so data survives reloads
                            // without the server.
                            dataDir: "idb://offline_notes",
                        }});
                        // Bootstrap the migration tracker before anything
                        // else, then apply pending migrations apply-once like
                        // sqlx does server-side (tracked in the client's meta
                        // table). Idempotent DDL keeps a half-applied set
                        // healable.
                        await db.exec(
                            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
                        );
                        for (const m of [{migs}]) {{
                            const key = "migration:" + m.name;
                            const seen = await db.query(
                                "SELECT 1 FROM meta WHERE key = $1", [key]);
                            if (seen.rows.length === 0) {{
                                await db.exec(m.up);
                                await db.query(
                                    "INSERT INTO meta (key, value) VALUES ($1, '1')",
                                    [key],
                                );
                            }}
                        }}
                        // The resolved instance is stored Rust-side (the
                        // thread_local singleton); this promise only exists
                        // to dedupe concurrent in-flight constructions.
                        return db;
                    }})();
                    // Drop a failed init so the next call retries cleanly.
                    globalThis.__pgliteReady.catch(() => {{
                        globalThis.__pgliteReady = null;
                    }});
                }}
                return globalThis.__pgliteReady;
            }})()"#
        );

        // Ok(value) = the auto-awaited construction promise resolving to the
        // PGlite instance; Err(value) = its rejection reason. Only a
        // successful construction is stored, so a failed init leaves the
        // singleton empty and the next call retries (mirroring the JS-side
        // `catch` that clears `__pgliteReady`).
        match eval_js(&code).await {
            Ok(instance) => {
                let p = Pglite { instance };
                INSTANCE.with(|i| *i.borrow_mut() = Some(p.clone()));
                Ok(p)
            }
            Err(e) => Err(JsValue::from_str(&error_text(&e))),
        }
    }

    /// Run a single statement with bound string parameters, returning the
    /// result object ({ rows: [...], fields: [...] }).
    pub async fn query(&self, sql: &str, params: &[String]) -> Result<JsValue, JsValue> {
        let q: Function = js_sys::Reflect::get(&self.instance, &"query".into())?.dyn_into()?;
        let arr = params
            .iter()
            .map(|p| JsValue::from_str(p))
            .collect::<js_sys::Array>();
        let ret = q.call2(&self.instance, &JsValue::from_str(sql), &arr.into())?;
        let promise: js_sys::Promise = ret.dyn_into()?;
        wasm_bindgen_futures::JsFuture::from(promise).await
    }
}

/// Extract a readable message from a JS error object.
fn error_text(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .or_else(|| js_sys::JSON::stringify(e).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| format!("{e:?}"))
}

/// Read rows out of a PGlite query result. Each row is an object keyed by
/// column name; only string values are extracted (our schema is text-only).
pub fn rows_of(result: &JsValue) -> Vec<js_sys::Object> {
    let arr: js_sys::Array = js_sys::Reflect::get(result, &"rows".into())
        .ok()
        .and_then(|v| v.dyn_into().ok())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|row| row.dyn_into::<js_sys::Object>().ok())
        .collect()
}

/// Get a string column from a row object.
pub fn str_field(row: &js_sys::Object, key: &str) -> Option<String> {
    js_sys::Reflect::get(row, &key.into()).ok()?.as_string()
}
