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
| `permission-menu.json` | Command permission labels drawn by real interactive `agy` 1.5.9, plus the two-choice file creation menu (also drawn when `write_file` overwrites an existing file); digits 1–3 run a scratch command without Enter, while digit 4 and Esc leave its target untouched; file creation accepts 1 and denies 2 | measured through the daemon attach stream; scratch command filename normalized to the OSC fixture's `probe.txt` |
| `composer-after-paced-deletes.bin` | real `agy` 1.5.9's raw output at 143 × 40 in Auto mode: the ready screen, a 145-character doorbell line typed in one write (echoed whole by byte 4954), then one DEL per write, 20 ms apart, for each character; it ends on a bare `>` composer with the cursor at its start, no mode hint, and `? for shortcuts` under it | captured byte for byte through a PTY, 2026-11-17; the scratch path in the line is the measurement's own, since deleted |

## Synthetic terminal geometry

The executable `agy` stand-in replays these hooks and OSC sequences without calling a model. Its composer follows the geometry measured on real `agy` 1.5.9 at 80 and 175 columns: the header starts at zero-based row 1, column 18 and leaves that row with a bare LF; the `> ` composer sits between two full-width `─` rules; its placeholder and `? for shortcuts` footer use brightblack. The cursor stays after the input text, or just after `> ` when empty, rather than below the footer. Long input word-wraps with a two-cell continuation indent, Ctrl+U leaves a blank composer without restoring the hint, and one DEL removes one character. Resize triggers a redraw beginning with `ESC[2J ESC[3J ESC[H`.

The stand-in omits the account banner, logo and mode announcement, and renders injected hook text into the PTY for delivery assertions. `delivered.txt` also records that text for diagnostics; `orchestration/route/003` checks the pane's PTY scrollback so a long work-done footer cannot hide an otherwise displayed task from the assertion.
