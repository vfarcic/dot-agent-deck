# Agent-Driven Notifications — Dogfood Setup

> **Developer / maintainer reference.** This page documents an internal development experiment on *this repo's own* orchestration and is intentionally excluded from the published documentation site. It is **not** a recommended practice for users — [PRD #126](../../prds/126-agent-driven-notifications.md) Phase 3 decides whether any of it graduates into published docs.

When you start a PRD run and walk away, the workflow halts at two explicit user gates — the test-plan approval (step 1) and the merge confirmation (step 7) of the `orchestrator` `prompt_template` — plus a long unattended stretch where `release` waits for CI and Greptile to settle. The terminal bell only reaches a focused terminal, so those waits are invisible once you leave the desk.

This setup makes the **agents** send the out-of-band signal themselves, using nothing but a shell script and this repo's `.dot-agent-deck.toml`. **No `dot-agent-deck` source code is involved** — that is the hard constraint of PRD #126, and it is checkable: `git diff --stat -- src/` on the PRD branch is empty. The point is to find out whether agent-driven notification works at all before deciding whether the deck should grow machinery for it.

## The moving parts

| Piece | Path | Committed? |
|---|---|---|
| Helper script | `scripts/notify.sh` | yes |
| Notify instructions | `.dot-agent-deck.toml` role `prompt_template`s | yes |
| Expectation log | `.dot-agent-deck/notify-log.md` | no — `.dot-agent-deck/` is gitignored |
| Destination | ntfy topic `dot-agent-deck-notify-0c0d15e13936d122` | topic name is in the script's default |

## The destination: ntfy

The channel is a [ntfy](https://ntfy.sh) topic, reached with a plain `curl` POST. ntfy was chosen over an MCP server because **this repo does not run one agent**: the orchestrator and auditor are Pi, the reviewer is OpenCode, the tester is Codex, and coder/release are Claude. An MCP has to be wired up per agent and may not be supported by all of them; a shell-callable CLI works uniformly for any agent that has a shell, with zero per-agent configuration. There is also nothing to provision — no account, no API key, no OAuth.

**The topic is public.** ntfy.sh topics are unauthenticated: anyone who knows the topic name can both read the stream and publish to it, and the name is committed in a public repo. The random suffix keeps it from being *guessed*, not from being *looked up*. So the script sends only workflow status — a role, an event kind, and one short human sentence. Never send secrets, tokens, diffs, or file contents through it. This is acceptable for a dogfood whose entire payload is "a gate was reached"; it would not be acceptable for anything else.

To point the setup somewhere else, set either environment variable before launching the deck — no code or config change needed:

```bash
export DOT_AGENT_DECK_NOTIFY_TOPIC=my-private-topic      # default: dot-agent-deck-notify-0c0d15e13936d122
export DOT_AGENT_DECK_NOTIFY_SERVER=https://ntfy.example # default: https://ntfy.sh
```

## Subscribing a device

The notification has to arrive somewhere that is **not** the terminal running the deck — that is the entire point. Pick one:

- **Phone**: install the ntfy app (iOS / Android / F-Droid), tap *Add subscription*, enter the topic name. This is the intended path.
- **Desktop browser**: open `https://ntfy.sh/dot-agent-deck-notify-0c0d15e13936d122` and allow notifications.
- **Another terminal** (useful for verifying the plumbing, but it does not test the walk-away case): `curl -s https://ntfy.sh/dot-agent-deck-notify-0c0d15e13936d122/json` streams arrivals as JSON lines.

Subscribe **before** starting a run. ntfy caches messages for a short window, but an unsubscribed topic is the easiest way to turn a successful send into an apparent miss and pollute the findings.

## The helper script

```bash
scripts/notify.sh <gate|done|blocked> <role> '<one-line message>'
```

It is **fire-and-forget by construction**: it always exits 0 — no network, no `curl`, no arguments, all exit 0 — and `curl` runs under `--max-time 5`. An agent therefore cannot be stalled or derailed by it, which is why every instruction says "send and continue". An orchestrator that blocks on a notification it cannot confirm is a worse failure than no notification at all.

The body is posted with `curl --data-raw`, never `--data-binary`: `--data-binary` gives a leading `@` the meaning "read this local file", so a perfectly ordinary message like `@/etc/passwd` — or any message that happens to start with a path or an @mention — would upload file contents to the public topic. `--data-raw` sends the `@` literally.

Every invocation **tries** to append **two** records to the expectation log, both carrying the same invocation id:

1. `reached` — written **before** the send is attempted, so it survives a dead network, a missing `curl`, or the agent being killed mid-send.
2. `send=ok` / `send=failed` / `send=skipped` — written **after** the attempt, with the HTTP status and `curl` exit code.

**Logging is best-effort, not guaranteed.** A read-only checkout, an invalid `DOT_AGENT_DECK_NOTIFY_LOG`, a permissions problem, or a full filesystem can leave one row or none while the script still exits 0 — the exit code is caller-facing and deliberately never reflects a logging failure. What it does instead is print `notify.sh: could not …` on **stderr** and, at the end, `notify.sh: N of 2 expectation-log rows were lost for <invocation id>`. So a missing row is only ambiguous if nobody read the agent's stderr; run the [pre-run check](#pre-run-check-the-log-actually-works) below and the ambiguity is gone before the run starts.

## Where each role fires

| Role | Moment | Kind |
|---|---|---|
| `orchestrator` | test plan posted, waiting for approval (step 1) | `gate` |
| `orchestrator` | merge confirmation reached (step 7) | `gate` |
| `orchestrator` | run finished or abandoned | `done` |
| `release` | PR checks + Greptile settled after the step-5 wait | `done` |
| `coder` / `tester` / `reviewer` / `auditor` | blocked, or missing critical context | `blocked` |

Each instruction is anchored inline at the role's real waiting point rather than collected in a preamble, so it reads at the moment it applies. The orchestrator additionally carries an explicit "do not notify on anything else" line — per-step chatter would make the signal worthless.

## The expectation log, and why it exists

`.dot-agent-deck/notify-log.md` is the instrumentation that makes this experiment **falsifiable**, and it is the load-bearing detail of the whole design.

The failure mode under test is the *absence* of a signal. When an agent forgets to notify, compacts the instruction away, or dies first, the observable outcome is **nothing** — which is indistinguishable from "working correctly, nothing to report yet". Without a local record, "it seemed fine" is the only available conclusion, and the PRD produces no negative evidence at all.

The log is a Markdown table, so it renders on GitHub and greps cleanly:

```markdown
| timestamp (UTC) | invocation | role | kind | record | detail |
|---|---|---|---|---|---|
| 2026-07-25T23:07:54Z | inv-6a6541ca-322ccc-50b3 | coder | gate | reached | helper script send path from a Claude worker shell |
| 2026-07-25T23:07:55Z | inv-6a6541ca-322ccc-50b3 | coder | gate | send=ok | http=200 topic=dot-agent-deck-notify-0c0d15e13936d122 |
```

The `invocation` column is what makes the pair correlatable. Two roles can run concurrently in this orchestration and a second-resolution timestamp is not a key — the same role can even fire twice within one second. Both rows of one invocation carry the same id, so `reached` is matched to its send outcome by **id**, never by adjacency or timestamp.

It is gitignored (via the blanket `.dot-agent-deck/` rule) because it is per-clone runtime state. Copy the relevant rows into the PRD's findings section — that is how the data leaves the machine.

### Pre-run check: the log actually works

Because logging is best-effort, confirm it works *before* a run rather than discovering a silent logging failure while reconciling. From the repo root:

```bash
./scripts/notify.sh done precheck 'pre-run log check'; echo "exit=$?"
log=${DOT_AGENT_DECK_NOTIFY_LOG:-.dot-agent-deck/notify-log.md}
tail -2 "$log"
```

Expected: `exit=0`, **no** `notify.sh:` line on stderr, and two rows at the end of `$log` — one `reached` and one `send=ok`, sharing one invocation id. If the rows are absent, or stderr carries a `notify.sh: could not …` line, fix the log path or its permissions before starting; otherwise every gap this run produces is uninterpretable.

If the log predates the `invocation` column (a five-column header), move it aside so the script writes a fresh header — mixing schemas breaks both the rendering and the greps.

## Reconciling after a run

Do this after **every** observed run, per PRD #126 M2.2. There are three headline counts — **moments reached**, **notifications attempted**, **notifications arrived** — and three gaps between them. Every one of them is **per run**: the log accumulates across runs, so a lifetime `grep -c` over the whole file answers the wrong question.

### Delimit the run first

Bound the run by **log line numbers** rather than by wall-clock timestamps: line numbers survive two runs that start in the same minute, and they are unambiguous when the pre-run check has just appended two rows of its own. Take the count *after* the pre-run check and *before* starting the deck:

```bash
log=${DOT_AGENT_DECK_NOTIFY_LOG:-.dot-agent-deck/notify-log.md}
wc -l < "$log"    # after the pre-run check, BEFORE starting the run -> $start
wc -l < "$log"    # AFTER the run ends                              -> $end
```

Note the wall-clock start and end too — not to slice the log, but to bound the device-arrival tally in gap 2, which has no line numbers.

Then slice that window once and count within it:

```bash
sed -n "$((start+1)),${end}p" "$log" > /tmp/run-window.md

grep -c '| reached |'      /tmp/run-window.md   # moments reached      <- headline count 1
grep -c '| send=ok |'      /tmp/run-window.md   # send=ok
grep -c '| send=failed |'  /tmp/run-window.md   # send=failed
grep -c '| send=skipped |' /tmp/run-window.md   # send=skipped
```

**Headline count 2, `notifications attempted`, = `send=ok` + `send=failed`.** `send=skipped` is explicitly *excluded* — no request left the machine — but it is still reported in its own column, because a run full of skips means the helper never had a transport at all.

**Headline count 3, `notifications arrived`, has no command.** It is the manual tally from the subscribed device described in gap 2 below; nothing on this machine can produce it.

The four `grep -c` results and the device tally are exactly the columns of the [per-run table](#the-record-to-append-to-the-prd).

For the compaction split (required — see below), take the line number at which the orchestrator compacted and slice the window into two sub-windows, counting each identically.

### The three gaps

1. **Reached but never sent** — an invocation id with a `reached` row and no `send=ok` row. The transport failed; the `send=failed` row names the HTTP status and `curl` exit code. List the offending ids:
   ```bash
   comm -23 \
     <(grep '| reached |'  /tmp/run-window.md | cut -d'|' -f3 | tr -d ' ' | sort) \
     <(grep '| send=ok |'  /tmp/run-window.md | cut -d'|' -f3 | tr -d ' ' | sort)
   ```
2. **Sent but never arrived** — `send=ok` rows with nothing on the subscribed device. Compare the log against what the ntfy app actually shows, and **tally arrivals by hand for the same window** (count the notifications whose timestamps fall between the run's start and end). This is the only gap the log cannot see by itself, which is why a device must be subscribed.
3. **Never even reached** — the gap the log records by *omission*, and the most important one. You know from the run itself that step 1 and step 7 happened; if there is no `reached` row for a gate that demonstrably occurred, the agent never ran the instruction. Reconstruct the expected moments from the run's actual shape, not from the log — the log cannot report its own absence. Check the agent's stderr too: a `notify.sh: could not …` line means the helper *did* run and only the logging failed, which is a different finding.

### The record to append to the PRD

Append one of these per run to the PRD's Findings → Counts and gaps section. The pre/post-compaction split is required by M2.2, so both rows are always present; write `n/a` in the post-compaction row if the run never compacted.

```markdown
#### Run <n> — PRD #<issue>, <YYYY-MM-DD>

Window: `.dot-agent-deck/notify-log.md` lines <start+1>–<end> (<HH:MM>Z–<HH:MM>Z). Compaction: <line number, or "none">.

| window | moments reached | attempted (ok+failed) | send=ok | send=failed | send=skipped | arrived on device | gap 1 reached-not-sent | gap 2 sent-not-arrived | gap 3 moment-with-no-row |
|---|---|---|---|---|---|---|---|---|---|
| pre-compaction | | | | | | | | | |
| post-compaction | | | | | | | | | |

- **Gap 1 invocation ids**: <ids, or none>.
- **Gap 3 reconstruction**: <the moments the run demonstrably reached that produced no `reached` row, and how you know they occurred>.
- **Notes**: <tripwire thoughts, stderr logging failures, anything that makes a count unreliable>.
```

Reconciliation is manual, and that is fine at this scale. Automating it would require deck code, which the constraint forbids.

### Expect notifications to stop after a compaction

The notify instruction lives in a `prompt_template`, which is delivered as a `Read` tool result and is therefore **compaction-mortal** — the exact mechanism [PRD #82](../../prds/82-orchestrator-role-reinforcement.md) documents. A clean before/after-compaction split in the log is evidence *for* #82's post-compaction re-assert, **not** evidence that agents are unreliable at notifying or that the deck needs notification machinery. That is why the per-run record above always carries two rows: note the log line number at which the orchestrator compacted, slice the run window there, and count each half separately so the two explanations stay distinguishable.

## Reproducing the setup from scratch

1. Pick an unguessable topic name: `echo "dot-agent-deck-notify-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')"`.
2. Verify the send path returns HTTP 200 from a shell — it must be the shell the agents actually get:
   ```bash
   curl -s -o /dev/null -w '%{http_code}\n' -d 'reachability check' https://ntfy.sh/<topic>
   ```
3. Set the topic as `notify.sh`'s default (or export `DOT_AGENT_DECK_NOTIFY_TOPIC`).
4. Verify from **each** agent this repo runs, not just from your own shell — see [Per-agent reachability checks](#per-agent-reachability-checks) below. Four different CLIs shell out four different ways; your own shell proves none of them.
5. Run the [pre-run log check](#pre-run-check-the-log-actually-works).
6. Subscribe a device to the topic.
7. Run a PRD and reconcile.

### Per-agent reachability checks

This repo runs **four** agent families, one per launcher in `devbox.json`. Every one of them carries a notify instruction, so every one of them has to be checked. The procedure is the same in each case — get the agent to shell out to the helper non-interactively and report the exit code — only the launcher differs:

| Role(s) | Launcher (`.dot-agent-deck.toml`) | Non-interactive form used for the check |
|---|---|---|
| `orchestrator`, `auditor` | `devbox run pi-big` | `pi -p --model <cheap> --approve '<prompt>'` |
| `coder`, `release` | `devbox run agent-orchestrator`, `devbox run agent-release` (Claude) | `claude -p --model haiku --allowedTools Bash`, prompt piped on **stdin** |
| `reviewer` | `devbox run oc-big` | `opencode run --model <cheap> '<prompt>'` |
| `tester` | `devbox run codex-big` | `codex exec --model <cheap> --sandbox workspace-write -c sandbox_workspace_write.network_access=true '<prompt>'` |

The prompt is the same everywhere — substitute the role name:

> Run this exact shell command from the repository root, then report ONLY the numeric exit code it returned: `./scripts/notify.sh gate <role> 'reachability check'`

Concretely:

```bash
devbox run -- pi -p --model anthropic/claude-haiku-4-5 --approve \
  "Run this exact shell command from the repository root, then report ONLY the numeric exit code it returned: ./scripts/notify.sh gate orchestrator 'reachability check'"

# Claude takes the prompt on stdin: under `devbox run --` a positional prompt is
# swallowed and `claude -p` exits with "Input must be provided either through stdin…".
echo "Run this exact shell command from the repository root, then report ONLY the numeric exit code it returned: ./scripts/notify.sh gate coder 'reachability check'" \
  | devbox run -- claude -p --model haiku --allowedTools Bash

devbox run -- opencode run --model openai/gpt-5.4-mini \
  "Run this exact shell command from the repository root, then report ONLY the numeric exit code it returned: ./scripts/notify.sh gate reviewer 'reachability check'"

devbox run -- codex exec --model gpt-5.6-sol --sandbox workspace-write \
  -c sandbox_workspace_write.network_access=true -c model_reasoning_effort=low \
  "Run this exact shell command from the repository root, then report ONLY the numeric exit code it returned: ./scripts/notify.sh gate tester 'reachability check'"
```

Use a cheap model in every case — you are testing shell reach, not reasoning. These are the non-interactive forms of the launchers the deck spawns interactively; the bash tool being exercised is the same one, and shell reach is the only thing at issue.

**After each command**, check the log rather than trusting the agent's self-report — an agent that says "0" without having run anything is exactly the failure this experiment is about:

```bash
tail -4 .dot-agent-deck/notify-log.md
```

The check passes only when all three hold:

1. the agent reported exit code **0**;
2. the log gained a `reached` row and a `send=ok http=200` row **sharing one invocation id**;
3. that pair carries the **expected `role`**.

Read four lines rather than two: an agent may invoke the helper more than once (OpenCode did, re-running the command to capture the exit code), which is precisely why the pairing is by invocation id and not by adjacency.

An agent that cannot be driven non-interactively is **not a blocker** — record "agent *X* cannot self-notify" as a finding in the PRD, which is exactly what the M1.1 success criterion asks for ("or the failure to reach one of them is recorded as a finding").

Codex is worth one note: it runs inside a PID namespace and reports a low PID (2), which is why the invocation id mixes in the epoch and `$RANDOM` rather than relying on the PID alone.

## Turning it off

The whole thing is one revertible commit. Remove the `scripts/notify.sh` lines from the `prompt_template`s in `.dot-agent-deck.toml` and the agents stop notifying immediately — running deck instances pick up config edits within a couple of seconds. The script and this note can stay; nothing calls them.
