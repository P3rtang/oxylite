# offline-notes

Offline-first notes app demonstrating the stack it could grow from into a small library:

| Piece | Tech |
|---|---|
| Client UI | Dioxus (wasm/web) |
| Client DB | PGlite, persisted in IndexedDB |
| Server | Axum + Tokio |
| Server DB | Postgres via sqlx |

## Architecture

```
┌────────────────────────────────┐         ┌─────────────────────────┐
│ Dioxus UI (wasm)               │         │ Axum server (tokio)     │
│   │                            │         │   /sync      (WS push/  │
│ sync engine (JSON over WS)     │◄─:3000──┤   /pglite/   (module)   │
│   one task; JS callbacks only  │         │   /*         (dist/)    │
│ enqueue, a session loop drains │         │   sqlx → Postgres       │
│ PGlite (idb://offline_notes)   │         │     (sync_log table)    │
└────────────────────────────────┘         └─────────────────────────┘
```

- Everything is **single-origin** on `http://localhost:3000`: the built dioxus
  client (dist), the vendored PGlite bundle (`/pglite/`), and the WS (`/sync`)
  are all served by the axum server.
- Writes go to **PGlite first** (offline works), then are **pushed** to the
  server; while offline they queue and flush on reconnect.
- The server appends every change to a `sync_log` table with a monotonic
  `seq` and streams changes to connected sockets (500 ms ticker) plus
  explicit `Pull`s (at-least-once; LWW upsert makes re-delivery harmless).
- The client persists its sync **cursor in PGlite** itself (`meta` table), so
  pulls resume after reloads.

Wire protocol (see `crates/shared`):

```json
C->S  {"type":"Pull","since":42}
C->S  {"type":"Push","ops":[{"table":"notes","id":"…","data":{…},"updated_at":"…"}]}
S->C  {"type":"Ack","cursor":57}
S->C  {"type":"Events","events":[…],"cursor":57}
```

## Setup

```bash
# one command: postgres + server + built client, opens the browser
./scripts/browser.sh
```

Manually, piece by piece:

```bash
podman compose up -d            # Postgres
./scripts/vendor-pglite.sh      # one-time: vendor PGlite 0.5.8 into assets
cargo build -p server           # axum + tokio + sqlx
DX=~/.cargo/bin/dx ./scripts/build-client.sh   # dioxus web build -> crates/client/dist
./scripts/serve-server.sh       # serves app + pglite + ws on :3000
```

Open http://localhost:3000, add notes, kill the server, keep writing —
everything persists locally in IndexedDB and syncs once the server returns.
Console logs are tagged `[boot]` / `[sync]` / `[pglite]` for DevTools.

## Tests

```bash
./scripts/e2e.sh                # Playwright suite (bun); manages the stack itself
```

Three specs: local add + reload persistence, cross-client sync, offline queue
flush on reconnect.

## Path to a library

1. **Sync protocol** — generic table names/serde shapes (currently `notes`).
2. **Conflict resolution** — LWW on `updated_at` now; pluggable merge next.
3. **Dead-socket detection** — app-level heartbeat instead of relying on
   socket state (Chrome keeps established WS open even when "offline").
4. **PGlite bindings** — `crates/client/src/pglite.rs` is standalone and reusable.
5. **Dioxus hooks** — wrap the sync engine in a `use_sync_server()` hook.

## Notes

- **Licensing** — the vendored PGlite bundle (`crates/client/assets/pglite/`,
  from `@electric-sql/pglite`) is Apache-2.0; its license text is vendored
  alongside it per §4a. The WASM-compiled PostgreSQL inside it carries the
  permissive PostgreSQL Licence.
- **Tokio does not run in the browser.** Client async work uses
  wasm-bindgen-futures / dioxus' web runtime; tokio is the server runtime.
- PGlite's `dataDir` **must** be `idb://...` — the default constructor is an
  in-memory filesystem.
