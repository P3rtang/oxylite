# AGENTS.md — working rules for AI agents (and humans, honestly)

An offline-first notes app (Dioxus/wasm + PGlite in IndexedDB, axum +
Postgres sync server) that doubles as the proving ground for a small
sync library. This file is the entry point for working here; the repo's
own docs go deeper — read before acting, don't guess.

## Ethos

- **Review-driven.** Work lands as numbered review items (#1…#27 in
  `docs/PLAN.md`); the human reviews, decisions are recorded verbatim.
  Don't freelance big design changes — propose, get the decision, then
  implement.
- **Docs are the source of truth.** `docs/PLAN.md` "Current state" is
  the authoritative "where are we". Keep it current; a stale doc is a
  bug. Specs (`docs/spec/`) must track the code — refresh on any
  protocol/shape change and mark the commit they reflect.
- **Consistency by construction.** One entry point for testing
  (`test.sh`), deterministic state for e2e (committed DB dump, restored
  every run), dist freshness owned by serve/build steps, enforcement in
  hooks (pre-commit sqlx refresh) rather than in vibes. When you add a
  step a human could forget, mechanize it.
- **Plain Rust, no abstraction theater.** SQL is generated, not
  abstracted away; errors are typed (`docs/spec/errors.md`); no bare
  `String` results; query errors are data (`LAST_ERROR`), not panics.
  Comments are lowercase, dense with *why*, em-dashes over parentheses.
- **Contracts, not snapshots.** Tests pin observable behavior with
  exact assertions. e2e is isolation-safe by construction: own browser
  context (fresh PGlite/IndexedDB), unique `Date.now()` stamps (the
  remote Postgres is shared across parallel workers), seed-note canaries
  so absence assertions can't pass vacuously.

## Tooling

| Thing | How |
|---|---|
| Test everything | `./test.sh` (same as `--full`: lint + e2e) |
| Lint only | `./test.sh --lint` — fmt --check, build, clippy host + wasm (`-D warnings`), `cargo test --workspace` |
| e2e only | `./test.sh --e2e [playwright args…]` — args pass through (spec paths, `--grep "…"`, `--trace on`) |
| After migration changes | `./test.sh --e2e --fresh` (rebuilds default state + committed dump). Migration changed without `--fresh`? That's the stale-schema class of bug. |
| Postgres | `podman compose up -d` (service `postgres`, `sync:sync@localhost:5432/offline_notes`); psql: `podman compose exec -T postgres psql -U sync -d offline_notes` |
| Dev loop | `scripts/dev.sh` (live server + client); `scripts/serve-server.sh` for server only |
| Client build | `scripts/build-client.sh` (dx; output `crates/client/dist`) |
| DB default state | `scripts/db-default.sh restore\|rebuild` — `restore` (fast) before every e2e; `rebuild` only after schema/seed changes |
| sqlx offline data | `.sqlx/` at workspace root; populated by live-mode builds; **pre-commit hook refreshes it and hard-fails if Postgres is down** (`--no-verify` bypass exists, but fixing the actual problem is the habit) |

Gotchas worth keeping: debug dx builds show a "Your app is being
rebuilt" interstitial (cosmetic); sqlx macros need their `.sqlx`
entries or database-less builds (CI, fresh clones) fail.

## Rust test layout

Tests are integration targets in `crates/<crate>/tests/*.rs`, never
inline `#[cfg(test)]` modules (review decision, #27 round 2 — sources
stay clean). Current inventory:

- `shared/tests/delete_semantics.rs` — delete/tombstone contracts in
  pure code + the in-memory reference model
- `sync/tests/sql_shapes.rs` — exact-shape pins for generated client SQL
- `server/tests/delete_contracts.rs` — sqlx::test against real
  Postgres (runtime queries, no macros — test targets don't own
  `.sqlx` entries)
- `server/tests/generated_sql_prepares.rs` — PREPARE-checks every
  generated client statement against the migrated schema (the browser
  is the only place they normally execute; a bad shape must fail CI,
  not a user)

## Long-running commands: the background-job plugin

The plain bash tool kills commands around ~120s — anything longer must
go through the `start_background_job` tool (custom opencode plugin,
`.opencode/plugins/long_commands.js`):

- **target mode**: `target` = `e2e` | `lint` | `full` (the `test.sh`
  modes), plus `args` for e2e passthrough (spec paths, `--grep`,
  `--trace on`)
- **cmd mode**: `cmd` for any arbitrary command (`cargo test -p server`,
  `scripts/db-default.sh rebuild`, psql one-offs)
- **peek**: `peek: '<id>'` (or `'all'`) — elapsed time + short tail for
  a running job, no wake, no queueing. The queue message carries the
  job `id`.
- **cmd mode naming**: `name:` overrides the auto-slug for cmd jobs —
  friendly ids for `peek` (`name: 'dump-rebuild'` →
  `dump-rebuild-<ts>.log`).
- **Conflicts**: a second stack job (e2e/full — they share the
  Playwright stack + the default-state Postgres restore) is REFUSED
  while one runs; lint can run alongside.
- **Stall timeout**: 30 min default (0 disables), `timeout_min`
  overrides — a hung job is killed and still wakes the session.
- **Wake quality**: failed jobs wake with a bounded "failure markers"
  section (✘/failed/error lines, ANSI-stripped) — act without reading
  the whole log.
- **Restart recovery**: job state is persisted, so a job that outlives
  an opencode restart (plugin edits require one!) still wakes the
  session it was queued from — including "finished while opencode was
  down" reports. PID reuse is detected via /proc starttime.
- Queue the job and **end your turn** — a message with the exit code
  and log tail arrives when it finishes; full log at
  `/tmp/opencode/<slug>-<id>.log`. Never sleep-poll.
- Rule of thumb: anything you'd expect to exceed ~60–90s is a
  background job. Editing the plugin requires restarting opencode.

## docs/ — session memory (untracked)

`.gitignore`d by design; the tracked root `README.md` is the public
one. Glob/grep tools respect the ignore — use `ls`/`read` for docs.

- `docs/README.md` — map of the folder + conventions (statuses,
  graduation path, verbatim decisions)
- `docs/PLAN.md` — **entry point**: current state, review index,
  known remaining work, process gotchas
- `docs/ROADMAP.md` — long-term backlog; items graduate into numbered
  reviews with an `impl/<item>.md` design doc
- `docs/spec/` — living specs (sync-protocol, errors, lib-boundary);
  must track the code
- `docs/history/` — archived review logs (#1–#22 verbatim) and
  superseded design docs

House rules for changes: update the owning `impl/` doc + `PLAN.md`
when a session moves state; append review-log entries rather than
rewriting history; don't commit `docs/` (it never will be — that's
the point).

## Commit conventions

Lowercase, area prefix + em-dash + detail, e.g. `engine: durable op
log — offline writes survive reload and leader death`. Never commit
without being asked, even when everything is green.
