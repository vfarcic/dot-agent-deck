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

Every invocation appends **two** records to the expectation log:

1. `reached` — written **before** the send is attempted, so it survives a dead network, a missing `curl`, or the agent being killed mid-send.
2. `send=ok` / `send=failed` / `send=skipped` — written **after** the attempt, with the HTTP status and `curl` exit code.

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
| timestamp (UTC) | role | kind | record | detail |
|---|---|---|---|---|
| 2026-07-25T22:40:33Z | coder | gate | reached | helper script send path from a Claude worker shell |
| 2026-07-25T22:40:34Z | coder | gate | send=ok | http=200 topic=dot-agent-deck-notify-0c0d15e13936d122 |
```

It is gitignored (via the blanket `.dot-agent-deck/` rule) because it is per-clone runtime state. Copy the relevant rows into the PRD's findings section — that is how the data leaves the machine.

## Reconciling after a run

Do this after **every** observed run, per PRD #126 M2.2. There are three counts and two gaps.

```bash
# moments reached, sends attempted-and-accepted, sends that failed
grep -c '| reached |'    .dot-agent-deck/notify-log.md
grep -c '| send=ok |'    .dot-agent-deck/notify-log.md
grep -c '| send=failed\|| send=skipped' .dot-agent-deck/notify-log.md
```

1. **Reached but never sent** — `reached` rows with no matching `send=ok`. The transport failed. The log names the HTTP status and `curl` exit code.
2. **Sent but never arrived** — `send=ok` rows with nothing on the subscribed device. Compare the log against what the ntfy app actually shows. This is the only gap the log cannot see by itself, which is why a device must be subscribed.
3. **Never even reached** — the gap the log records by *omission*, and the most important one. You know from the run itself that step 1 and step 7 happened; if there is no `reached` row for a gate that demonstrably occurred, the agent never ran the instruction. Reconstruct the expected moments from the run's actual shape, not from the log — the log cannot report its own absence.

Reconciliation is manual, and that is fine at this scale. Automating it would require deck code, which the constraint forbids.

### Expect notifications to stop after a compaction

The notify instruction lives in a `prompt_template`, which is delivered as a `Read` tool result and is therefore **compaction-mortal** — the exact mechanism [PRD #82](../../prds/82-orchestrator-role-reinforcement.md) documents. A clean before/after-compaction split in the log is evidence *for* #82's post-compaction re-assert, **not** evidence that agents are unreliable at notifying or that the deck needs notification machinery. Record before- and after-compaction counts separately so the two explanations stay distinguishable.

## Reproducing the setup from scratch

1. Pick an unguessable topic name: `echo "dot-agent-deck-notify-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')"`.
2. Verify the send path returns HTTP 200 from a shell — it must be the shell the agents actually get:
   ```bash
   curl -s -o /dev/null -w '%{http_code}\n' -d 'reachability check' https://ntfy.sh/<topic>
   ```
3. Set the topic as `notify.sh`'s default (or export `DOT_AGENT_DECK_NOTIFY_TOPIC`).
4. Verify from **each** agent this repo runs, not just from your own shell — the agents differ. For the Pi orchestrator: `devbox run -- pi -p --model anthropic/claude-haiku-4-5 --approve 'Run ./scripts/notify.sh gate orchestrator "reachability check" and report the exit code'`. Use a cheap model; you are testing shell reach, not reasoning.
5. Subscribe a device to the topic.
6. Run a PRD and reconcile.

## Turning it off

The whole thing is one revertible commit. Remove the `scripts/notify.sh` lines from the `prompt_template`s in `.dot-agent-deck.toml` and the agents stop notifying immediately — running deck instances pick up config edits within a couple of seconds. The script and this note can stay; nothing calls them.
