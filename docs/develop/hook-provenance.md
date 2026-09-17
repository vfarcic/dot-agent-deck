# Hook-socket provenance

Issue [#1077](https://github.com/vfarcic/dot-agent-deck/issues/1077). This page is the threat model for the per-spawn hook capability token: what it ties together, what it defends against, and — stated before anything else so nobody cites it as more than it is — what it does **not** defend against. The mechanism lives in [`src/hook_provenance.rs`](../../src/hook_provenance.rs), whose module docs carry the same argument next to the code.

## What it is not

**It is not a boundary against a deliberate same-uid adversary.** The token has to be readable by the legitimate sender, and the legitimate sender is a process running as you. On Linux, one process's environment is readable by other processes of the same uid through `/proc/<pid>/environ`. That was measured on the development box this was written on rather than assumed: with YAMA `ptrace_scope = 1`, a same-uid process that was neither ancestor nor descendant of the target could still read the target's `environ` and find the `DOT_AGENT_DECK_*` keys in it. A sibling agent that reads a victim pane's environment gets the victim's token and can then forge everything the gate checks. `hidepid=` on `/proc` does not change this — it hides processes belonging to *other* users.

Nothing inside this deck's reach closes that. Any mechanism whose secret is delivered to the agent falls the same way, and the alternatives that deliver no secret are covered below and do not work either. Genuine separation needs the agents to run under different uids, which the deck does not do.

## The problem it does address

Every message on the hook socket names a `pane_id`, and before this change that name was the whole of the claim. The CLI copies `DOT_AGENT_DECK_PANE_ID` into the field, and the daemon acted on it. Consequences, all pre-existing:

- `delegate` naming an orchestrator's pane makes the daemon write and submit a task prompt into **every worker pane of that orchestration**.
- `work-done` naming a worker's pane makes the daemon write feedback into **the orchestrator's pane** — a different pane from the one the message claims to come from.
- PRD #220's dispatch return route is one-shot, so a forged completion **spends** it and the real unit's later completion resolves to nothing.

The socket is owner-only, which keeps other OS users out. It does not separate the several agents the deck deliberately runs under one uid, and those agents can read every pane id off the daemon's own `list-agents` / `daemon status` output.

## The mechanism

1. **Mint.** Every `AgentPtyRegistry::spawn_agent` mints 256 bits from the operating system (`getrandom`), keeps it on the agent's `RunningAgent`, and injects it as `DOT_AGENT_DECK_PANE_CAPABILITY` beside the pane id and agent id. A caller-supplied value is stripped, not honoured, so no two records can carry one token. The variable's name deliberately avoids `KEY`, `SECRET` and `TOKEN`: Codex's `shell_environment_policy` strips names matching `*KEY*`/`*SECRET*`/`*TOKEN*` from its shell tool whenever `ignore_default_excludes = false`, and a capability that never reaches the agent's shell turns every legitimate `work-done` from that agent into a silent refusal. A respawn mints a fresh one. The inherited value is also scrubbed from the base environment, so a daemon started from inside another deck's pane does not pass that deck's token on.
2. **Present.** The CLI reads the variable and puts it in the `token` field of every `DaemonMessage` it sends — all seven verbs: `delegate`, `work_done`, `get_seed`, `dispatch`, `list_targets`, `restart_role`, `spawn_role`.
3. **Check.** `run_hook_loop` classifies every `DaemonMessage` before any handler runs. It resolves **token → record → pane** and compares that pane with the one the message names.

The direction of step 3 is the point. The token is not an extra field checked against the claim; it *is* the claim. So an agent holding a perfectly valid token of its own still cannot name a sibling's pane.

| message presents | the daemon | verdict |
| --- | --- | --- |
| a token minted for the pane it names | admits it | `Attested` |
| a token minted for a **different** pane | refuses | `WrongPane` |
| a well-formed token this daemon never minted | refuses | `UnknownToken` |
| a value not shaped like a token | refuses | `Malformed` |
| no token, for a pane this daemon issued one | refuses (see the policy below) | `Missing` |
| no token, for a pane this daemon never issued one | admits it, as before | `Unattested` |

A refused `delegate`, `restart_role`, `spawn_role` or `list_targets` gets an `error` on the connection. A refused `get_seed` gets the same `{"seed":null}` as "nothing pending", so the pane's real occupant still receives its seed through the PTY-injection safety net. `work_done` and `dispatch` read no reply, so their refusal is a `warn!` in the daemon log and nothing else — **the sender is not told**. The log line names the verb, the claimed pane and the reason, and never the token. The daemon does not publish the token anywhere else either — it is held on `RunningAgent`, not on the `AgentRecord` clients receive — but an agent that prints its own environment puts its token into its pane's scrollback, which any attach client can snapshot.

### Why "was this pane issued a token" is not derived from the live records

A pane can be momentarily without any record while still holding authority: `respawn_agent_for_pane` removes the old record before `spawn_agent` inserts the new one, the daemon's role maps survive that, and a `clear = true` role respawns on every delegation. An answer derived from the live records would read that window as "never issued" and admit a token-less forgery landing in it. So the registry records each pane in a separate set at the moment its token is minted, under the same lock that reserves the spawn, and never prunes it.

### Why `Unattested` is not the obvious hole

It is reached only for a pane id this daemon has never issued a token for. In the daemon, the role maps that carry delegate and work-done authority are written only by `AppState::register_orchestration_role`, and each of its callers invokes it only after its spawn returned `Ok` — which is after the pane was recorded as issued. So a message landing in `Unattested` names a pane that holds no role. That was established by enumerating the write sites; a new writer of those maps that does not go through a spawn would break it.

## Why not `SO_PEERCRED`

It is the obvious reach, and it does not answer the question.

- **The uid is the wrong discriminator.** Every agent the deck runs has the same uid — that is the threat. The socket is already `0600`, so a uid check separates nothing the filesystem does not already separate.
- **The pid cannot be resolved to a pane in time.** The peer pid is unforgeable, and a walk up the process tree could in principle find which pane's PTY the sender lives under. But `work-done` and `dispatch` are fire-and-forget: the CLI connects, writes one line and exits, so by the time the daemon has the line the sender is routinely gone. A check that answers "cannot tell" for a legitimate sender has to fail open, and one that fails open is not a check — an adversary exits first.
- **When it does answer, it is weaker.** It costs a `ps` sample per message, is defeated by `setsid` and by re-parenting to init, and a same-uid adversary controls its own process tree.

## The `DOT_AGENT_DECK_HOOK_PROVENANCE` knob

Set on the **daemon**. Anything other than exactly `warn` (case-insensitive) is `enforce`, including a typo — a switch that weakens a check must not flip by accident.

`warn` moves exactly one verdict: `Missing` is admitted, with a per-message `warn!` naming the pane. It exists for the one situation in which a legitimate sender produces `Missing`: the `dot-agent-deck` binary invoked inside a pane is **older** than the daemon that spawned it, so it does not know to forward the token the daemon put in its environment. That happens when a daemon is started from a build other than the one on `PATH`. The refusal message names this variable so whoever hits it can act.

It does **not** relax `WrongPane`, `UnknownToken` or `Malformed`. None of them can be produced by an older CLI — an older CLI sends no token at all — so tolerating them would buy compatibility with nothing and re-open the forgery the gate exists to stop.

## Compatibility

The wire change is additive and moves no `PROTOCOL_VERSION`, which versions the attach socket rather than the hook socket: `token` is `#[serde(default, skip_serializing_if = "Option::is_none")]` on every payload, an older daemon ignores the key, and a CLI with no token emits exactly the JSON it always did. What changed is what a newer daemon **does** with a missing token, which is a semantic break and is recorded as one in `changelog.d/1077.breaking.md`.

| CLI | daemon | result |
| --- | --- | --- |
| new | new | enforced |
| new | old | works — the old daemon ignores `token` |
| old | old | works, unchanged |
| old | new | `Missing` is refused; set `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` on the daemon, or put the matching binary on the pane's `PATH` |

An agent that survives a daemon restart holds a token the new daemon never minted, so its messages are refused as `UnknownToken`. That changes what the log says rather than what happens: the role maps died with the old daemon, so those messages already had nowhere to route.

## Tests

The harness has a long-standing pattern of running the real CLI **from the test process** with just a pane id, standing in for an agent. That is, by construction, exactly the forgery the gate refuses, and the tests that do it now say so by calling `TuiDeckBuilder::impersonating_pane_signals()`, which sets the `warn` policy for that deck. The gate itself runs under the default policy in:

- `src/hook_provenance.rs` — the whole decision matrix and the policy, against a stub directory.
- `src/agent_pty.rs` — minting, injection, stripping a planted value, re-minting on respawn, the requirement outliving a removed record, and the token never reaching `AgentRecord`.
- `src/daemon.rs` `hook_provenance_*` — the real `run_hook_loop` over a real socket and registry: an attested `delegate` still routes, and a token-less one, a sibling's valid token and a foreign token are each refused without reaching the worker.
- `tests/hook_provenance.rs` — the real CLI forwards the token from its environment and omits the key when it has none.
- `orchestration/provenance/001` (`tests/e2e_hook_provenance.rs`, lane 1) — the whole chain through the real binary: a worker whose own command the daemon spawned reports twice, and a forgery sent between those two reports from outside the pane never reaches the orchestrator.

## What stays open

- **Same-uid `/proc/<pid>/environ` reading**, above — [#1129](https://github.com/vfarcic/dot-agent-deck/issues/1129), which also covers the fact that a refused `work-done` or `dispatch` is invisible to the command that sent it.
- **Raw `AgentEvent` traffic** — hook events and `agent-event` — is not gated. It is a published schema that third-party producers emit, its producers are hook scripts already written into each agent's own configuration by existing installations, and `apply_event` auto-registers an unknown `SessionStart`. Issues [#543](https://github.com/vfarcic/dot-agent-deck/issues/543) and [#401](https://github.com/vfarcic/dot-agent-deck/issues/401) track it.
