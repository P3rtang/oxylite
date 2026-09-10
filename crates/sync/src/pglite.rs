use core::cell::RefCell;
use js_sys::{Function, Reflect};
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

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
    /// the client's `meta` table, mirroring sqlx's `_sqlx_migrations`).
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
        let code = format!("({})([{}])", PGLITE_BOOT_JS, migs.join(","));

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
/// A text field off a raw row. Timestamp columns are `timestamptz`
/// (migration 0007), so PGlite hands those back as JS `Date` objects —
/// normalized to the canonical ISO form here, once, at the read boundary
/// (`Date.toISOString()` is exactly the protocol's canonical shape).
/// Everything else must already be text.
pub fn str_field(row: &js_sys::Object, key: &str) -> Option<String> {
    let value = js_sys::Reflect::get(row, &key.into()).ok()?;
    if value.as_string().is_some() {
        return value.as_string();
    }
    if value.is_instance_of::<js_sys::Date>() {
        return Some(js_sys::Date::from(value).to_iso_string().into());
    }
    None
}
