# Antigravity CLI (`agy`) payloads (PRD #211)

Hook payloads, hook outputs and OSC 7501 status sequences captured from a real Antigravity CLI for the deterministic `agy` tests. Everything here was captured on 2026-11-16 from **`agy` 1.5.9** (Linux x86-64) with `--model gemini-3.8-flash-low`, signed in with a consumer Google account, unless the table says otherwise.

**How it was captured.** `agy` ran in a PTY (120×40, `TERM=xterm-256color`) under a scratch `HOME` holding only a copy of the sign-in token and the seeds the report lists, with `AGY_CLI_OSC7501=on` and `AGY_CLI_DISABLE_AUTO_UPDATE=true`. A probe hook registered under one named key in the scratch `~/.gemini/config/hooks.json` appended each hook's stdin to a log; the PTY driver recorded every byte `agy` wrote. The measurement report with the raw evidence is `.dot-agent-deck/g211-agy-measurements.md` in the PRD #211 worktree; it is not committed.

**Sanitizing.** The scratch home is replaced with `/home/user`, the working folder with `/home/user/project` (`/home/user/untrusted` for the untrusted-folder capture), conversation ids with `00000000-0000-4000-8000-00000000000N`, tool-call ids with `call_000…N`, and the local proxy port in `stop-error.json` with `127.0.0.1:9`. No token, email or account name is kept. Field names, values and types are otherwise as captured; keys are sorted and the JSON is re-indented.

| File | Event | Source |
| --- | --- | --- |
| `session-start.json` | `SessionStart`, fired when the first prompt of the conversation was submitted | captured |
| `pre-invocation.json` | `PreInvocation`, first model call of a turn (`invocationNum` 0) | captured |
| `post-invocation.json` | `PostInvocation`, same turn | captured |
| `pre-tool-use-run-command.json` | `PreToolUse` for `run_command` (`echo agy-probe-1`) | captured |
| `post-tool-use-run-command.json` | `PostToolUse` for the same call, `error` empty | captured |
| `pre-tool-use-write-file.json` | `PreToolUse` for `write_file` (`Path`, `Content`) | captured |
| `post-tool-use-write-file.json` | `PostToolUse` for the same call | captured |
| `stop.json` | `Stop` at the end of an ordinary turn, `terminationReason` `NO_TOOL_CALL` | captured |
| `stop-before-continue.json` | `Stop` the probe answered with `output-stop-continue.json` (`executionNum` 0) | captured |
| `pre-invocation-after-continue.json` | the `PreInvocation` of the re-entered loop: `invocationNum` 1, `lastUserInput` still the user's prompt | captured |
| `stop-after-continue.json` | the `Stop` that ended the re-entered loop (`executionNum` 1, `continuationActive` still `false`) | captured |
| `stop-error.json` | `Stop` after the model request failed (network refused), `terminationReason` `ERROR` with the error text | captured |
| `session-start-untrusted-headless.json` | `SessionStart` from `agy -p` in a folder not in `trustedWorkspaces`: global hooks ran there | captured |
| `output-stop-continue.json` | a `Stop` hook output `agy` honoured: the model replied with the token the `reason` asked for | written by the probe, behaviour captured |
| `output-pre-invocation-inject-steps.json` | a `PreInvocation` output `agy` honoured: the nested `systemMessage` object reached the model | written by the probe, behaviour captured |
| `output-pre-invocation-inject-steps-rejected.json` | the string form of `systemMessage`, which `agy` rejected (`failed to unmarshal result … via protojson`) and ignored | written by the probe, behaviour captured |
| `osc7501-sequences.json` | every distinct OSC 7501 sequence `agy` wrote (`sequences`, with the exact bytes and the decoded `msg`), and the order they came in for each scenario (`scenarios`) | captured; `scenarios` transcribed from the driver's event log |
