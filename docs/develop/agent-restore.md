# Agent restore

How the deck resumes an interrupted agent into its conversation (PRD #1790 stage 3). The user page, [`docs/interrupted-work.md`](../interrupted-work.md), says what a person sees; the journal side (the verdicts, the checks a restore makes, the lines it writes) is in [`work-journal.md`](work-journal.md#restoring-an-agent-prd-1790-stage-3). This page is the launch half: how the resume arguments reach the agent without the deck touching the command the user wrote.

## The launch shim carries the resume

The deck never edits a recorded command: it was the user's own shell text, and a command can be a script, a `devbox run …`, or a wrapper that starts the agent somewhere inside it. The resume goes through the same `PATH` shim telemetry uses (`src/telemetry/shim.rs`), which sits in front of the agent's program however it is started.

- **Installed whatever telemetry says.** The daemon writes the shim scripts into `<state dir>/telemetry/bin` at start (`telemetry::start_from`), on Unix, for `claude`, `codex` and `devin`. With telemetry off that directory goes on no ordinary spawn's `PATH`; a restore puts it on its own spawn's.
- **One spawn, one request.** A restore spawn's environment carries `DOT_AGENT_DECK_RESUME_AGENT` (the program the agent's `ResumeSpec` names: `claude`, `codex`), `DOT_AGENT_DECK_RESUME_SESSION` (the conversation id) and `DOT_AGENT_DECK_RESUME_TOKEN` (32 hex characters from OS randomness), and `PATH` with the shim directory first (`agent_restore::start`). Telemetry's own `PATH` addition is skipped when the shim directory already leads (`exporter::prepare_run`).
- **In the shim.** When the program it stands in for is the one the request names, the shim removes the three variables from the program's environment (so nothing the agent starts is resumed into the same conversation) and either puts the agent's resume arguments in front of the user's own, as separate argv entries (`--resume <id>` for Claude Code, `resume <id>` for Codex, ahead of the wrapper's embedded-mode pair), and creates `<state dir>/telemetry/resume-acks/<token>`; or, for an id that is not of the agent's syntax or user arguments that choose another conversation or command (Claude's `-c`, `--continue`, `-r`, `--resume`, `--session-id`, `--fork-session`, `-p`, `--print`; any Codex subcommand or positional prompt), writes `<token>.refused` holding the reason and runs the program unchanged. The acknowledgements are created exclusively, without following a symlink, owner-only in an owner-only directory, and a token that already has one is not resumed again.
- **The daemon confirms; the acknowledgement only explains.** The restore succeeds when the new agent's first attested hook session is the requested id. The acknowledgement files are read for two things only: a `.refused` one ends the wait early, and at the deadline, whether the plain one exists tells `timed_out` (the shim applied the resume and no confirmation came) from `shim_not_reached` (nothing reached the shim). Either way the agent is stopped before anything is posted to it.

## What the shim cannot reach

A launch that does not pass through the shim is never resumed; the confirmation catches it and the restore answers `not-resumed`, offering Start over:

- the agent called by an absolute path, or `PATH` replaced, or a directory holding the real agent put in front of the shim's, by the command or a script it runs;
- a wrapper that drops the arguments it was given, or changes `HOME` or `CLAUDE_CONFIG_DIR` so the id names no conversation it can find (`session_mismatch` or `timed_out`);
- Windows, where there is no shim, and the daemon does not advertise `restore-agents`.

There is no fallback that types a resume command into a started agent (the 2026-10-27 decision keeps it as a fallback only; it would leave a blank agent while the deck waits for readiness, and would have to drive each agent's picker).

## Which agents

The table is `AgentSpec.resume` in `src/agent_registry.rs`: Claude Code and Codex carry a `ResumeSpec`; OpenCode, Pi, Devin and Antigravity do not, so their verdict is `start_over` with `agent-cannot-resume`, and the user page names what each is missing (CLAUDE.md rule 20). Codex stays listed as resumable only while its real-agent test passes.

## Hazards worth knowing

- **Claude Code's `--resume <id>` doubles as a search term**: an id it cannot find may open a picker rather than exit. No matching `SessionStart` arrives, so the confirmation times out and the agent is stopped, but the pane shows the picker until then.
- **Claude Code's `bypassPermissions` falls back to the default mode on resume**, so a restored agent may stop at permission prompts it did not stop at before.
- **A prompt on the original command line** would be sent again as a new turn on resume. Codex refuses it in the shim (a positional prompt is a conflict); for Claude Code the shim does not know every flag's arity, so it is not detected, which is acceptable because deck-started agents take their task through the inbox.
- **A same-uid client can send the request with the experimental flag off**: the daemon serves the verbs whatever the flag says. A request carrying a pane claim is refused, which keeps a cooperative agent from restoring agents; it is not an authentication boundary.
