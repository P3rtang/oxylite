use core::cell::RefCell;
use js_sys::{Function, Reflect};
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

use crate::contract::from_row::Row;

/// The PGlite data dir inside IndexedDB (IdbFs) — the epoch gate's
/// database derives its name from this (`<dir>-epoch`), so the compat
/// envelope and the data store can never drift apart unnoticed.
pub const DATA_DIR: &str = "offline_notes";

/// Why the local DB's schema epoch refused this bundle (#34): the stored
/// IDB version (the GENERATED migration count) is newer than the bundle
/// knows, or another tab holds the upgrade blocked. Distinct from
/// BridgeError — the engine's response is policy (one guarded reload,
/// then a banner), not a retry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum CompatError {
    #[error("stored schema epoch {stored} is newer than this bundle's {bundle}")]
    StaleBundle { stored: u32, bundle: u32 },
    #[error("epoch upgrade blocked — another tab holds an older connection")]
    Blocked,
    #[error("epoch gate failed: {0}")]
    Js(String),
}

/// The local-DB compatibility gate (#34): open a tiny epoch database with
/// the GENERATED idb version (the migration list's length — monotonic by
/// construction) BEFORE PGlite boots. IDB's built-in versioning does the
/// arbitration: a fresh or higher version upgrades cleanly (`onupgrade
/// needed`); a stored version newer than the requested one throws
/// `VersionError` — this wasm predates the local DB it cannot read.
/// A separate database from PGlite's own IdbFs store, so the gate never
/// fights PGlite's storage internals.
pub async fn ensure_local_compat(data_dir: &str, bundle_version: u32) -> Result<(), CompatError> {
    use wasm_bindgen::prelude::Closure;

    let window = web_sys::window().ok_or_else(|| CompatError::Js("no window".into()))?;
    let factory = window
        .indexed_db()
        .map_err(|e| CompatError::Js(format!("indexed_db unavailable: {e:?}")))?
        .ok_or_else(|| CompatError::Js("indexed_db null".into()))?;

    let name = format!("{data_dir}-epoch");
    let req = factory
        .open_with_u32(&name, bundle_version)
        .map_err(|e| CompatError::Js(format!("open failed: {e:?}")))?;

    let result: std::rc::Rc<std::cell::RefCell<Option<Result<u32, CompatError>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    // One-shot resolve/reject closures; the promise resolves when the
    // request settles.
    let (resolve, reject, promise) = {
        let mut resolve_cb = None;
        let mut reject_cb = None;
        let p = js_sys::Promise::new(&mut |res, rej| {
            resolve_cb = Some(res);
            reject_cb = Some(rej);
        });
        (resolve_cb.unwrap(), reject_cb.unwrap(), p)
    };

    // onupgradeneeded fires on fresh/upgrade: create the epoch store (an
    // empty database is legal, but a store gives future epoch records a
    // home) and let the version land.
    {
        let req_cb = req.clone();
        let on_upgrade = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            let req = req_cb.clone();
            if let Ok(Ok(db)) = req.result().map(|v| v.dyn_into::<web_sys::IdbDatabase>()) {
                let _ = db.create_object_store("epoch");
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        req.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));
        on_upgrade.forget();
    }

    // onsuccess: the database is open at the requested version (or was
    // upgraded to it). Close immediately — the gate is a check, not a
    // lease; PGlite opens its own store next.
    {
        let result = result.clone();
        let resolve = resolve.clone();
        let on_success = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            *result.borrow_mut() = Some(Ok(bundle_version));
            let _ = resolve.call0(&JsValue::NULL);
        }) as Box<dyn FnMut(web_sys::Event)>);
        req.set_onsuccess(Some(on_success.as_ref().unchecked_ref()));
        on_success.forget();
    }

    // onerror: read the request's DOMException — VersionError is the
    // stale-bundle signature.
    {
        let req_cb = req.clone();
        let result = result.clone();
        let reject = reject.clone();
        let on_error = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            let req = req_cb.clone();
            *result.borrow_mut() = Some(Err(map_idb_error(&req, bundle_version)));
            let _ = reject.call0(&JsValue::NULL);
        }) as Box<dyn FnMut(web_sys::Event)>);
        req.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        on_error.forget();
    }

    // onblocked: another tab holds an older connection open during the
    // upgrade. The gate is leader-only, and the previous leader's document
    // death releases its connection — so this is transient; the engine
    // may retry.
    {
        let result = result.clone();
        let reject = reject.clone();
        let on_blocked = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            *result.borrow_mut() = Some(Err(CompatError::Blocked));
            let _ = reject.call0(&JsValue::NULL);
        }) as Box<dyn FnMut(web_sys::Event)>);
        req.set_onblocked(Some(on_blocked.as_ref().unchecked_ref()));
        on_blocked.forget();
    }

    JsFuture::from(promise)
        .await
        .map_err(|e| CompatError::Js(format!("gate promise rejected: {e:?}")))?;
    match result.borrow().as_ref() {
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => Err(e.clone()),
        None => Err(CompatError::Js("gate settled without a result".into())),
    }
}

fn map_idb_error(req: &web_sys::IdbOpenDbRequest, bundle_version: u32) -> CompatError {
    // The stored version is only visible via the error's message text
    // ("less than the existing version (N)") — parse it when present so
    // the error carries the numbers, not just the shape.
    let dom = req.error().ok().flatten();
    match dom.as_ref().map(|d| d.name()) {
        Some(name) if name == "VersionError" => {
            let stored = dom
                .map(|d| d.message())
                .and_then(|m| {
                    m.rsplit('(')
                        .next()
                        .and_then(|s| s.trim_end_matches(')').parse().ok())
                })
                .unwrap_or(0);
            CompatError::StaleBundle {
                stored,
                bundle: bundle_version,
            }
        }
        Some(name) => CompatError::Js(name.to_string()),
        None => CompatError::Js("unknown idb error".into()),
    }
}

/// Rust bridge to the vendored PGlite (the official ESM bundle copied
/// verbatim into `crates/client/assets/pglite/`). No JS of our own is
/// maintained: we instantiate PGlite's own artifact and call its public
/// `query`/`exec` API through wasm-bindgen.
///
/// PGlite persists its data dir in IndexedDB via its IdbFs
/// (`dataDir: "idb://offline_notes"`), so writes survive page reloads and
/// keep the app functional while the server is unreachable.
/// The bootstrap snippet, included at compile time and eval'd as one
/// expression: `(boot)([migrations])`. Kept in a real .js file for
/// syntax highlighting and lintability; it only touches JS globals
/// (`__pgliteReady` promise cache), so there is no boundary to keep in
/// sync — the migrations argument arrives as a JSON array.
static PGLITE_BOOT_JS: &str = include_str!("../assets/pglite-boot.js");

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
    /// `migrations` before anyone can touch it.
    ///
    /// Singleton lifecycle: once a construction succeeds, `INSTANCE` holds
    /// an owned `Pglite` and every later call returns a cheap clone —
    /// callers can hand `&Pglite` around without re-initializing. `init`
    /// never returns `None`-take semantics: the UI calls it from many
    /// tasks, so re-joining the live instance is the point.
    ///
    /// `new PGlite(...)` must happen in JS — wasm-bindgen `call` cannot
    /// construct ES classes — so this is driven by the eval'd boot snippet
    /// (`assets/pglite-boot.js`). The snippet keeps its in-flight promise
    /// on `globalThis.__pgliteReady` so two Rust tasks racing `init`
    /// before either has a resolved instance share ONE construction
    /// instead of opening two emscripten modules on the same IndexedDB
    /// dir. wasm-bindgen async externs auto-await the returned promise.
    ///
    /// The boot snippet applies `migrations` itself (tracked apply-once in
    /// the lib's own schema — `SCHEMA.meta` — mirroring sqlx's
    /// `_sqlx_migrations`).
    pub async fn init(migrations: &[(&'static str, &'static str)]) -> Result<Pglite, BridgeError> {
        if let Some(existing) = INSTANCE.with(|i| i.borrow().clone()) {
            return Ok(existing);
        }

        // The server serves the vendored bundle at /pglite/ with proper MIME
        // types. A dynamic import inside eval() can only resolve absolute
        // URLs, so the snippet builds the full URL from the document
        // location.
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
        let code = format!(
            "({})({}, [{}])",
            PGLITE_BOOT_JS,
            serde_json::to_string(crate::SCHEMA).unwrap(),
            migs.join(",")
        );

        // Ok(value) = the auto-awaited construction promise resolving to the
        // PGlite instance; Err(value) = its rejection reason. Only a
        // successful construction is stored, so a failed init leaves the
        // singleton empty and the next call retries (mirroring the JS-side
        // `catch` that clears `__pgliteReady`).
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
            Err(e) => Err(BridgeError::from_rejection(&e)),
        }
    }

    /// Run a single statement with bound string parameters, returning the
    /// result object ({ rows: [...], fields: [...] }).
    pub async fn query(&self, sql: &str, params: &[String]) -> Result<JsValue, BridgeError> {
        let q: Function = Reflect::get(&self.instance, &"query".into())
            .map_err(|e| BridgeError::from_rejection(&e))?
            .dyn_into()
            .map_err(|e: JsValue| BridgeError::from_rejection(&e))?;
        let arr = params
            .iter()
            .map(|p| JsValue::from_str(p))
            .collect::<js_sys::Array>();
        let ret = q
            .call2(&self.instance, &JsValue::from_str(sql), &arr.into())
            .map_err(|e| BridgeError::from_rejection(&e))?;
        let promise: js_sys::Promise = ret
            .dyn_into()
            .map_err(|e: JsValue| BridgeError::from_rejection(&e))?;
        JsFuture::from(promise)
            .await
            .map_err(|e| BridgeError::from_rejection(&e))
    }
}

/// The common JS↔Rust error exchange: every rejection crossing the bridge
/// is normalized into this shape, so Rust call sites get typed errors
/// instead of opaque `JsValue`s (and no prose-parsing anywhere).
///
/// String payloads here are the wire format itself — JS rejections carry
/// display text, and this is the boundary where it crosses. Serializable:
/// failures of subordinate tabs' requests travel over BroadcastChannel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum BridgeError {
    /// PGlite surfaced a Postgres error: the SQLSTATE code plus the
    /// message (e.g. code 23505, unique violation).
    #[error("postgres {code}: {message}")]
    Postgres { code: String, message: String },
    /// A JS `Error` (or any thrown value) without a Postgres code.
    #[error("{message}")]
    Js { message: String },
}

impl BridgeError {
    /// Normalize a JS rejection: strings pass through; error objects are
    /// reflected for `message` and a Postgres SQLSTATE (`code`, falling
    /// back to `cause.code` — PGlite nests the PG error); anything else
    /// is debug-printed. Never panics; unknown shapes degrade to `Js`.
    pub(crate) fn from_rejection(e: &JsValue) -> Self {
        if let Some(s) = e.as_string() {
            return Self::Js { message: s };
        }
        let obj = match e.dyn_ref::<js_sys::Object>() {
            Some(o) => o,
            None => {
                return Self::Js {
                    message: format!("{e:?}"),
                };
            }
        };
        let message = Reflect::get(obj, &"message".into())
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| format!("{e:?}"));
        let code = Reflect::get(obj, &"code".into())
            .ok()
            .and_then(|v| v.as_string())
            .or_else(|| {
                Reflect::get(obj, &"cause".into())
                    .ok()
                    .and_then(|c| Reflect::get(&c, &"code".into()).ok())
                    .and_then(|v| v.as_string())
            });
        match code {
            Some(code) => Self::Postgres { code, message },
            None => Self::Js { message },
        }
    }
}

/// Read rows out of a PGlite query result. Each row is an object keyed by
/// column name, wrapped as the lib's `Row` (typed readers live there).
pub fn rows_of(result: &JsValue) -> Vec<Row> {
    let arr: js_sys::Array = js_sys::Reflect::get(result, &"rows".into())
        .ok()
        .and_then(|v| v.dyn_into().ok())
        .unwrap_or_default();
    arr.iter().map(Row::from_value).collect()
}
