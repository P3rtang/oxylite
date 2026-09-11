// Long-command runner: hands the repo's slow test commands (or any
// arbitrary shell command) to background processes so the chat frees up
// immediately, then wakes the session with the exit code and log tail
// when the job finishes — no sleep-polling, no 120s tool timeouts.
// The contract is immutable: queue → end turn → ONE wake on finish.
// Everything else serves that contract:
// - `peek` verb: progress (elapsed + tail) without a wake or a queue.
// - stack-conflict guard: two Playwright runs would trample the shared
//   default-state Postgres, so a second stack job is refused.
// - stall timeout: a hung job is killed and STILL wakes the session.
// - failure markers: on a failed exit, the wake message carries the
//   matching log lines (ANSI-stripped, bounded) — act without reading
//   the whole log.
// - restart recovery: job state (pid, log, session) is persisted, so a
//   job that outlives an opencode restart (plugin edits require one!)
//   still wakes the session it was queued from. PID reuse is detected
//   via /proc starttime; wakes land in the persisted session even after
//   a restart (visible when that session is resumed).
import { tool } from '@opencode-ai/plugin';

const JOBS = {
    e2e: './test.sh --e2e',
    lint: './test.sh --lint',
    full: './test.sh --full',
};

const TAIL_OK = 12;
const TAIL_FAIL = 30;
const DEFAULT_TIMEOUT_MIN = 30;
const PEEK_TAIL = 5;
const STATE_FILE = '/tmp/opencode/long-commands-state.json';
const ORPHAN_POLL_MS = 5_000;
const ORPHAN_MAX_AGE_MS = 24 * 60 * 60 * 1000;
const FAIL_MARKERS = 15;
const FAIL_RE = /^\s*[✗✘×✖]|fail(?:ed|ure)|error\[|error:|panicked|Refused/i;

// e2e and full both run the Playwright stack, which starts by restoring
// the committed default DB state — two at once trample each other's
// Postgres. lint is stack-free. cmd-mode commands are classed by
// content: a test.sh --e2e/--full passthrough counts as stack too.
const stackClass = (target, cmd) =>
    target === 'e2e' || target === 'full' || /test\.sh\s+(--e2e|--full)/.test(cmd ?? '')
        ? 'stack'
        : null;

const stripAnsi = (s) => s.replace(/\x1b\[[0-9;]*m/g, '');

// The process start time from /proc — a stable identity across PID
// reuse: a recycled pid has a different starttime, so a stale entry
// never pretends a new process is our job.
const procStart = async (pid) => {
    try {
        const stat = await Bun.file(`/proc/${pid}/stat`).text();
        // stat: pid (comm) state ppid …; fields after ") " start at
        // state (= field 3), so starttime (field 22) is index 19.
        return stat.split(') ')[1]?.split(' ')[19] ?? null;
    } catch {
        return null;
    }
};

const procAlive = async (pid, start) => {
    const now = await procStart(pid);
    return now !== null && now === start;
};

export const AsyncTaskPlugin = async ({ client }) => {
    // One job per key; a second call while running just returns the log.
    const running = new Map();

    // Serialized state-file mutations (the event loop interleaves the
    // await points of concurrent executes otherwise). persist(entry)
    // upserts; persist(null, id) removes.
    let stateChain = Promise.resolve();
    const withState = (fn) => {
        stateChain = stateChain.then(fn).catch(() => {});
        return stateChain;
    };
    const persist = (entry, removeId) =>
        withState(async () => {
            const state = await Bun.file(STATE_FILE)
                .json()
                .then((j) => (Array.isArray(j.jobs) ? j.jobs : []))
                .catch(() => []);
            const next = state.filter((j) => j.id !== (removeId ?? entry?.id));
            if (entry) next.push(entry);
            await Bun.write(STATE_FILE, JSON.stringify({ jobs: next }));
        });

    // The one wake path — a plain injected message, no queueing. Both
    // the live watcher and the restart recovery go through this.
    const wake = async (sessionID, text, level = 'info') => {
        await client.app
            .log({
                body: {
                    service: 'long-commands',
                    level,
                    message: text.split('\n')[0].slice(0, 200),
                },
            })
            .catch(() => {});
        await client.session
            .prompt({
                path: { id: sessionID },
                body: { parts: [{ type: 'text', text }] },
            })
            .catch(async (e) => {
                await client.app.log({
                    body: {
                        service: 'long-commands',
                        level: 'error',
                        message: `wake-up prompt failed: ${e}`,
                    },
                });
            });
    };

    const tail = (log, lines) =>
        Bun.file(log)
            .text()
            .then((t) => t.trim().split('\n').slice(-lines).join('\n'))
            .catch(() => '(log unreadable)');

    const failMarkers = async (log) => {
        const text = await Bun.file(log)
            .text()
            .then(stripAnsi)
            .catch(() => '');
        const found = text.split('\n').filter((l) => FAIL_RE.test(l)).slice(0, FAIL_MARKERS);
        return found.length ? found.join('\n') : '(none matched — read the full log)';
    };

    // Restart recovery: adopt jobs persisted by a previous plugin
    // instance. Entries whose process is already gone finished while
    // opencode was down — wake immediately (no exit code available, the
    // log tells the story). Live ones get a poll watcher; the wake path
    // is the same as a fresh job's. Stale entries (older than a day)
    // are pruned.
    withState(async () => {
        const state = await Bun.file(STATE_FILE)
            .json()
            .then((j) => (Array.isArray(j.jobs) ? j.jobs : []))
            .catch(() => []);
        const live = state.filter((j) => Date.now() - j.started < ORPHAN_MAX_AGE_MS);
        for (const orphan of live) {
            if (await procAlive(orphan.pid, orphan.start)) {
                const poll = setInterval(async () => {
                    if (await procAlive(orphan.pid, orphan.start)) return;
                    clearInterval(poll);
                    const tailText = await tail(orphan.log, TAIL_FAIL);
                    await persist(null, orphan.id);
                    await wake(
                        orphan.sessionID,
                        `Background job '${orphan.key}' (id ${orphan.id}) finished after an opencode restart.\n` +
                            `Full log: ${orphan.log}\nLast lines:\n${tailText}\n\n` +
                            'Read the full log if needed, then continue the task.',
                    );
                }, ORPHAN_POLL_MS);
            } else {
                const tailText = await tail(orphan.log, TAIL_FAIL);
                await persist(null, orphan.id);
                await wake(
                    orphan.sessionID,
                    `Background job '${orphan.key}' (id ${orphan.id}) finished while opencode was down.\n` +
                        `Full log: ${orphan.log}\nLast lines:\n${tailText}\n\n` +
                        'Read the full log if needed, then continue the task.',
                );
            }
        }
        if (live.length !== state.length) {
            await Bun.write(
                STATE_FILE,
                JSON.stringify({ jobs: state.filter((j) => live.includes(j)) }),
            );
        }
    });

    return {
        tool: {
            start_background_job: tool({
                description:
                    "Starts a long-running command (test suites or any shell command) in the background and returns immediately. " +
                    "When the job finishes, a message with its exit code and log tail is injected into this session automatically — " +
                    "so queue the job and END YOUR TURN instead of sleep-polling. " +
                    "Use this for anything that might exceed the ~120s shell tool limit. " +
                    "Pass `peek` instead of target/cmd to check a running job (elapsed time + log tail) without queuing anything.",
                args: {
                    target: tool.schema
                        .enum(['e2e', 'lint', 'full'])
                        .optional()
                        .describe(
                            'e2e = Playwright suite, lint = fmt + clippy + unit tests, full = lint and e2e both. ' +
                                'Mutually exclusive with `cmd`.',
                        ),
                    cmd: tool.schema
                        .string()
                        .optional()
                        .describe(
                            'Arbitrary shell command to run from the repo root, e.g. ' +
                                '`bun run test tests/sync/delete.spec.ts --trace on`. Takes precedence over `target`.',
                        ),
                    args: tool.schema
                        .string()
                        .optional()
                        .describe(
                            'Extra CLI args appended after the base command (target mode only). ' +
                                'e2e/full: passed through to playwright (spec paths, `--grep "…"`, `--trace on`). ' +
                                'lint: ignored. Example: `--grep "syncs to a second client"`',
                        ),
                    name: tool.schema
                        .string()
                        .optional()
                        .describe(
                            'Friendly job id for cmd mode (e.g. `rebuild-default-state`) — used by `peek` and the wake-up. ' +
                                'Defaults to a slug of the command.',
                        ),
                    peek: tool.schema
                        .string()
                        .optional()
                        .describe(
                            "Check mode — starts nothing. A job id (from the queue message) or 'all': " +
                                'returns matching running jobs with elapsed time, timeout budget, and a short log tail.',
                        ),
                    timeout_min: tool.schema
                        .number()
                        .optional()
                        .describe(
                            'Stall timeout in minutes; a hung job is killed and reported as timed out. ' +
                                'Default 30 (nothing legitimate runs that long), 0 disables.',
                        ),
                },
                async execute(args, context) {
                    // Peek: status only, never queues, never wakes.
                    if (args.peek != null) {
                        const query = args.peek.trim();
                        const entries = [...running.entries()];
                        if (entries.length === 0) return 'No jobs running.';
                        const matches = entries.filter(
                            ([key, job]) =>
                                query === 'all' || job.id === query || key.includes(query),
                        );
                        if (matches.length === 0) {
                            return `No job matches '${query}' — running: ${entries.map(([, j]) => j.id).join(', ')}.`;
                        }
                        const out = [];
                        for (const [key, job] of matches) {
                            const min = ((Date.now() - job.started) / 60000).toFixed(1);
                            const budget =
                                job.timeoutMin > 0
                                    ? `, timeout at ${job.timeoutMin} min`
                                    : ', no timeout';
                            out.push(
                                `${job.id} — '${key}' — running ${min} min${budget} (log: ${job.log})\n${await job.tail()}`,
                            );
                        }
                        return out.join('\n');
                    }

                    const { target, cmd } = args;
                    if (!target && !cmd) {
                        return 'Provide either `target` (e2e/lint/full) or `cmd`.';
                    }
                    const extra = args.args?.trim() ?? '';
                    const base = cmd ?? JOBS[target];
                    const name = cmd ? `cmd: ${cmd}` : target;
                    const jobKey = !cmd && extra ? `${target}: ${extra}` : name;
                    const existing = running.get(jobKey);
                    if (existing) {
                        return `Job '${jobKey}' is already running (id ${existing.id}, log: ${existing.log}). Suspend activity — the result message will arrive when it finishes.`;
                    }

                    const cls = stackClass(target, cmd);
                    if (cls) {
                        for (const [, job] of running) {
                            if (job.cls === cls) {
                                return (
                                    `Refused: a ${cls} job is already running (id ${job.id}: ${job.key}) — ` +
                                    'e2e/full share the Playwright stack and the default-state Postgres restore, ' +
                                    'so concurrent runs would trample each other. ' +
                                    `Peek with peek: '${job.id}' or wait for the wake-up.`
                                );
                            }
                        }
                    }

                    const slug =
                        args.name?.trim().replace(/[^a-zA-Z0-9_-]+/g, '-').replace(/^-|-$/g, '') ||
                        (cmd
                            ? cmd.replace(/[^a-zA-Z0-9]+/g, '-').slice(0, 40).replace(/^-|-$/g, '') ||
                              'cmd'
                            : target);
                    const id = `${slug}-${Date.now()}`;
                    const log = `/tmp/opencode/${id}.log`;
                    const full = cmd ? base : `${base}${extra ? ` ${extra}` : ''}`;
                    // Brace group: the redirection must capture the WHOLE
                    // command — without it, a compound cmd's redirection
                    // binds to its last command only and the earlier
                    // segments' output is lost (caught by the marker test).
                    const proc = Bun.spawn({
                        cmd: ['bash', '-c', `{ ${full}; } > ${log} 2>&1`],
                        cwd: context.worktree || context.directory,
                        stdout: 'ignore',
                        stderr: 'ignore',
                    });
                    proc.unref();

                    const start = await procStart(proc.pid);
                    const timeoutMin = args.timeout_min ?? DEFAULT_TIMEOUT_MIN;
                    const job = {
                        id,
                        key: jobKey,
                        cls,
                        log,
                        started: Date.now(),
                        timeoutMin,
                        timedOut: false,
                        tail: () => tail(log, PEEK_TAIL),
                    };
                    running.set(jobKey, job);
                    persist({
                        id,
                        key: jobKey,
                        pid: proc.pid,
                        start,
                        log,
                        started: job.started,
                        sessionID: context.sessionID,
                    });

                    // Stall timeout: nothing legitimate runs 30 minutes;
                    // a hung job must still wake the session (SIGTERM,
                    // SIGKILL fallback if the tree ignores it).
                    let timeoutTimer = null;
                    if (timeoutMin > 0) {
                        timeoutTimer = setTimeout(() => {
                            if (!running.has(jobKey)) return;
                            job.timedOut = true;
                            try {
                                proc.kill();
                            } catch {}
                            setTimeout(() => {
                                try {
                                    proc.kill('SIGKILL');
                                } catch {}
                            }, 10_000);
                        }, timeoutMin * 60_000);
                    }

                    await client.app.log({
                        body: {
                            service: 'long-commands',
                            level: 'info',
                            message: `started ${jobKey} (id ${id}, pid ${proc.pid}) -> ${log}`,
                        },
                    });

                    // Fire-and-forget: when the job exits, wake this
                    // session with the result. The agent's turn has long
                    // ended by then (jobs run minutes, turns run seconds).
                    (async () => {
                        const code = await proc.exited;
                        if (timeoutTimer) clearTimeout(timeoutTimer);
                        running.delete(jobKey);
                        persist(null, id);
                        const notice = job.timedOut
                            ? ` (timed out after ${timeoutMin} min — killed)`
                            : '';
                        const outcome = job.timedOut
                            ? 'The job hung and was killed. Inspect the log tail for where it stalled.'
                            : code === 0
                              ? 'Read the full log if needed, then continue the task.'
                              : 'Failure markers point at the cause; read the full log for the rest, then continue the task.';
                        const tailText = await tail(log, code === 0 ? TAIL_OK : TAIL_FAIL);
                        // Timed-out jobs diagnose from the tail (where it
                        // stalled); marker matching there is pure noise.
                        const markers =
                            code === 0 || job.timedOut
                                ? ''
                                : `\nFailure markers (first ${FAIL_MARKERS}):\n${await failMarkers(log)}\n`;
                        await wake(
                            context.sessionID,
                            `Background job '${jobKey}' (id ${id}) finished with exit code ${code}${notice}.\n` +
                                `Full log: ${log}\nLast lines:\n${tailText}\n${markers}\n${outcome}`,
                            code === 0 ? 'info' : 'error',
                        );
                    })();

                    return `Job '${jobKey}' queued (id: ${id}, log: ${log}). Suspend activity — end your turn now; a message with the exit code and log tail will arrive in this session when the job finishes.`;
                },
            }),
        },
    };
};
