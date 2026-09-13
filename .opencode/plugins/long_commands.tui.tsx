/** @jsxImportSource @opentui/solid */
// TUI side of the long-commands plugin — visual indicator for background
// jobs (server side: long_commands.js, shared state: the jobs JSON file).
// - sidebar: a "Jobs" section in the sidebar stack (below Modified Files,
//   order 600, LSP-section conventions: bold header + bullet rows)
// - /jobs: palette command + slash command → dialog listing running jobs
// Polls the state file the server plugin already maintains; no IPC, no
// server-plugin changes. Edits require an opencode restart to load.
// Note: the section is only visible while the sidebar is shown (<leader>b
// toggles it); /jobs works regardless. app_bottom was rejected — it
// steals a full terminal row and sits below the prompt.
import { createSignal, For, Show } from "solid-js"
import type { TuiPlugin, TuiPluginApi, TuiPluginModule } from "@opencode-ai/plugin/tui"

const ID = "long-commands"
const STATE_FILE = "/tmp/opencode/long-commands-state.json"
const POLL_MS = 1_000
const KEY_MAX = 48

type Job = {
    id: string
    key: string
    log: string
    started: number
    pid?: number
    sessionID?: string
}

const readJobs = async (): Promise<Job[] | null> =>
    Bun.file(STATE_FILE)
        .json()
        .then((j) => (Array.isArray(j?.jobs) ? (j.jobs as Job[]) : null))
        .catch(() => null)

const fmt = (ms: number) => {
    const s = Math.max(0, Math.floor(ms / 1000))
    const m = Math.floor(s / 60)
    const h = Math.floor(m / 60)
    if (h > 0) return `${h}h${String(m % 60).padStart(2, "0")}m`
    if (m > 0) return `${m}m${String(s % 60).padStart(2, "0")}s`
    return `${s}s`
}

const clip = (s: string, max = KEY_MAX) => (s.length > max ? `${s.slice(0, max - 1)}…` : s)

function Section(props: { api: TuiPluginApi; jobs: () => Job[]; elapsed: (j: Job) => string }) {
    const theme = () => props.api.theme.current
    return (
        <Show when={props.jobs().length > 0}>
            <box>
                <box flexDirection="row" gap={1}>
                    <text fg={theme().text}>
                        <b>Jobs</b>
                    </text>
                </box>
                <For each={props.jobs()}>
                    {(j) => (
                        <box flexDirection="row" gap={1}>
                            <text flexShrink={0} style={{ fg: theme().success }}>
                                •
                            </text>
                            <text fg={theme().textMuted} wrapMode="none" truncate>
                                {clip(j.key)} — {props.elapsed(j)}
                            </text>
                        </box>
                    )}
                </For>
            </box>
        </Show>
    )
}

const tui: TuiPlugin = async (api) => {
    const [jobs, setJobs] = createSignal<Job[]>([])
    const [now, setNow] = createSignal(Date.now())

    const poll = async () => {
        // a failed read (mid-write JSON, missing file) keeps the last
        // known list — a real finish is the entry actually disappearing
        const next = await readJobs()
        if (next) setJobs(next)
        setNow(Date.now())
    }
    await poll()
    const timer = setInterval(poll, POLL_MS)
    api.lifecycle.onDispose(() => clearInterval(timer))

    const elapsed = (j: Job) => fmt(now() - j.started)

    const openJobs = () => {
        if (jobs().length === 0) {
            api.ui.toast({ variant: "info", message: "No background jobs running." })
            return
        }
        api.ui.dialog.replace(() =>
            api.ui.DialogSelect({
                title: "Background jobs",
                skipFilter: true,
                // live getters — the dialog keeps updating while open
                get options() {
                    return jobs().map((j) => ({
                        title: `${clip(j.key)} — ${elapsed(j)}`,
                        value: j.id,
                        description: j.log,
                    }))
                },
                onSelect() {
                    api.ui.dialog.clear()
                },
            }),
        )
    }

    api.slots.register({
        // below Modified Files (500) — last section in the sidebar stack
        order: 600,
        slots: {
            sidebar_content() {
                return <Section api={api} jobs={jobs} elapsed={elapsed} />
            },
        },
    })

    api.keymap.registerLayer({
        commands: [
            {
                name: "jobs.list",
                slashName: "jobs",
                title: "Background jobs",
                category: "Session",
                namespace: "palette",
                run: openJobs,
            },
        ],
    })
}

const plugin: TuiPluginModule & { id: string } = {
    id: ID,
    tui,
}

export default plugin
