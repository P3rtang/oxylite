// Long-command runner: hands the repo's slow test commands (or any
// arbitrary shell command) to background processes so the chat frees up
// immediately, then wakes the session with the exit code and log tail
// when the job finishes — no sleep-polling, no 120s tool timeouts.
import { tool } from '@opencode-ai/plugin';

const JOBS = {
    e2e: './test.sh --e2e',
    lint: './test.sh --lint',
    full: './test.sh --full',
};

const TAIL_LINES = 12;

export const AsyncTaskPlugin = async ({ client }) => {
    // One job per key; a second call while running just returns the log.
    const running = new Map();

    return {
        tool: {
            start_background_job: tool({
                description:
                    "Starts a long-running command (test suites or any shell command) in the background and returns immediately. " +
                    "When the job finishes, a message with its exit code and log tail is injected into this session automatically — " +
                    "so queue the job and END YOUR TURN instead of sleep-polling. " +
                    "Use this for anything that might exceed the ~120s shell tool limit.",
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
                },
                async execute(args, context) {
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
                        return `Job '${jobKey}' is already running (log: ${existing.log}). Suspend activity — the result message will arrive when it finishes.`;
                    }

                    const slug = cmd
                        ? cmd.replace(/[^a-zA-Z0-9]+/g, '-').slice(0, 40).replace(/^-|-$/g, '') || 'cmd'
                        : target;
                    const log = `/tmp/opencode/${slug}-${Date.now()}.log`;
                    const full = cmd ? base : `${base}${extra ? ` ${extra}` : ''}`;
                    const proc = Bun.spawn({
                        cmd: ['bash', '-c', `${full} > ${log} 2>&1`],
                        cwd: context.worktree || context.directory,
                        stdout: 'ignore',
                        stderr: 'ignore',
                    });
                    proc.unref();
                    running.set(jobKey, { proc, log });

                    await client.app.log({
                        body: {
                            service: 'long-commands',
                            level: 'info',
                            message: `started ${jobKey} (pid ${proc.pid}) -> ${log}`,
                        },
                    });

                    // Fire-and-forget: when the job exits, wake this
                    // session with the result. The agent's turn has long
                    // ended by then (jobs run minutes, turns run seconds).
                    (async () => {
                        const code = await proc.exited;
                        running.delete(jobKey);
                        const tail = await Bun.file(log)
                            .text()
                            .then((t) =>
                                t.trim().split('\n').slice(-TAIL_LINES).join('\n'),
                            )
                            .catch(() => '(log unreadable)');
                        await client.app.log({
                            body: {
                                service: 'long-commands',
                                level: code === 0 ? 'info' : 'error',
                                message: `${jobKey} exited ${code}`,
                            },
                        });
                        await client.session
                            .prompt({
                                path: { id: context.sessionID },
                                body: {
                                    parts: [
                                        {
                                            type: 'text',
                                            text:
                                                `Background job '${jobKey}' finished with exit code ${code}.\n` +
                                                `Full log: ${log}\n` +
                                                `Last lines:\n${tail}\n\n` +
                                                `Read the full log if needed, then continue the task.`,
                                        },
                                    ],
                                },
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
                    })();

                    return `Job '${jobKey}' queued (log: ${log}). Suspend activity — end your turn now; a message with the exit code and log tail will arrive in this session when the job finishes.`;
                },
            }),
        },
    };
};
