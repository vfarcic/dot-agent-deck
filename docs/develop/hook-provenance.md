# Hook-socket provenance

Issues [#1077](https://github.com/vfarcic/dot-agent-deck/issues/1077) and [#1129](https://github.com/vfarcic/dot-agent-deck/issues/1129). This page is the threat model for the per-spawn hook capability token: what it ties together, what it defends against, and — stated before anything else so nobody cites it as more than it is — what it does **not** defend against. It is also where the deck's **trust boundary** is recorded, because #1129 settled it: see [The trust boundary](#the-trust-boundary-the-os-user). The mechanism lives in [`src/hook_provenance.rs`](../../src/hook_provenance.rs), whose module docs carry the same argument next to the code.

## What it is not

**It is not a boundary against a deliberate same-uid adversary.** The token has to be readable by the legitimate sender, and the legitimate sender is a process running as you. On Linux, one process's environment is readable by other processes of the same uid through `/proc/<pid>/environ`. That was measured on the development box this was written on rather than assumed: with YAMA `ptrace_scope = 1`, a same-uid process that was neither ancestor nor descendant of the target could still read the target's `environ` and find the `DOT_AGENT_DECK_*` keys in it. A sibling agent that reads a victim pane's environment gets the victim's token and can then forge everything the gate checks. `hidepid=` on `/proc` does not change this — it hides processes belonging to *other* users.

Nothing inside this deck's reach closes that. Any mechanism whose secret is delivered to the agent falls the same way, and the alternatives that deliver no secret are covered below and do not work either. Genuine separation needs the agents to run under different uids, which the deck does not do.

## The trust boundary: the OS user

**Decided (issue #1129): the deck's trust boundary is the operating-system user. Every agent it runs is inside that boundary and is trusted with respect to every other agent it runs.** This is a policy, not a measurement — it holds because it was chosen, and it stays true until someone chooses otherwise. What follows is the argument, because a decision recorded without its reasoning is worth much less than one that can be re-examined.

The operating rule that falls out of it, stated so it can be quoted: **do not put an agent you would not trust with your whole working environment into a deck alongside agents you do.** Not "into the same orchestration" — into the same *deck*, and in practice into the same login session, because nothing below the uid separates them.

### Why the boundary sits there rather than at the agent

**Because above that boundary the adversary model stops being coherent.** An agent that would read a sibling's `/proc/<pid>/environ` to steal its capability token is a process with your shell, your checkout, your `~/.ssh`, your `~/.claude` and `~/.codex` credentials, your `git` remotes and your ability to write any file you can write. Forging a `delegate` into a sibling's workers is not the cheapest thing available to it — it is one of the *most* expensive. The same process can rewrite the project's `.dot-agent-deck.toml`, edit the hook scripts in the other agent's own configuration, replace the `dot-agent-deck` binary on `PATH`, or simply `kill` the sibling. A boundary that stops the expensive attack and leaves the cheap ones open is not a boundary; calling it one is the thing this page exists to prevent.

That is the same conclusion [#401](https://github.com/vfarcic/dot-agent-deck/issues/401) reached from the other end — "any process able to write to that socket already has code execution as the user, so it can do considerably worse than redirect keystrokes" — and it is that issue's option 1, taken deliberately rather than by default.

**What the boundary does not excuse.** It is a statement about a *deliberate* adversary, and the non-adversarial half is still a correctness obligation. An accidental forgery — a stale `DOT_AGENT_DECK_PANE_ID` inherited by an unrelated process, a recycled pane id, an agent that outlived the daemon that spawned it — is a bug whether or not anyone is attacking, and it is the case that actually happens. That is the half the token closes and keeps closing (see [The mechanism](#the-mechanism)); "same-uid agents are trusted" is not a licence to stop checking that a message came from the spawn it claims.

### Why not a uid per agent

It is the one option that separates the agents at the kernel's own boundary rather than confining one route to a token they can all still read, and it is **not** proposed. Not because it is merely large, but because it changes what the product is: a single-user developer tool becomes a multi-tenant agent runner, and every one of the following is an entry-price item rather than a follow-up.

- **A privileged component appears.** The deck today runs wholly unprivileged as you. Allocating uids needs a setuid helper, a root daemon, or user namespaces with `newuidmap`/`newgidmap` and a `/etc/subuid` allocation — a component with more privilege than the thing it supervises, on every platform separately.
- **The socket's permission model goes.** `0600` on the hook and attach sockets is what keeps other users out. With agents under other uids it has to become a per-agent socket or `0660` plus a shared group — and at that point `SO_PEERCRED`'s uid genuinely *does* become a discriminator, which is worth knowing, since [the argument against it below](#why-not-so_peercred) rests on every agent sharing one uid.
- **Credentials do not divide.** Each agent CLI reads its own config and credentials out of `$HOME` — `~/.claude`, `~/.codex`, `~/.config/opencode`. A per-agent uid needs a per-uid `HOME`, so either the user's single credential is copied into N homes (which *lowers* the bar rather than raising it: N copies of one secret, each readable by a uid that did not have it before) or each uid needs its own subscription, which the user does not have.
- **The filesystem stops agreeing.** Dispatched units work in git worktrees the daemon creates as you, in a repository owned by you. Git refuses a repository owned by another uid unless `safe.directory` says otherwise, and shared write access across uids means ACLs or a shared group on every path the deck writes — worktrees, `.dot-agent-deck/`, the state dir, the log. [`worktree-ownership.md`](worktree-ownership.md) is the existing surface that assumes one owner.
- **The supporting tooling assumes one uid too.** The e2e temp-root reaper decides by `kill(2)` liveness against pids it does not own ([`e2e-temp-dirs.md`](e2e-temp-dirs.md)); `daemon stop` resolves its peer through `SO_PEERCRED` ([`src/platform/peercred/`](../../src/platform/peercred/)).
- **It is Linux-shaped.** The unprivileged route above — user namespaces plus a `/etc/subuid` allocation — is a Linux facility. On macOS creating a user account needs administrator rights and there is no user-namespace equivalent; Windows has no uid at all. Each would need its own design and its own argument.

**The trigger for revisiting is a product change, not a security insight.** If the deck ever runs an agent *on someone else's behalf* — a hosted deck, a shared runner, an agent supplied by a third party — the boundary above is no longer the right one and the uid split is the entry price. Nothing learned about `/proc` changes that; a change in who the agents belong to does.

### Why not an LSM or sandbox profile

Landlock, AppArmor, SELinux or an agent's own sandbox could deny one agent read access to another's `/proc/<pid>/environ`, and that is genuinely narrower and cheaper than a uid split. It is still not a boundary, for reasons that compound rather than alternate:

- **It is per-platform, and the deck ships for three.** Landlock and AppArmor are Linux facilities; macOS and Windows would each need a separate mechanism, and this page would then have to be read per-OS.
- **The deck does not own the sandbox.** The deck spawns the command the user configured, and the confinement that command runs under is the agent's own — Codex's sandbox policy, Claude's permission model. Making the deck wrap each agent in a profile means the deck deciding what that agent may read, including the repositories and credentials it exists to use.
- **It closes one route to the token, not the capability.** An agent that prints its own environment puts its token into its pane's scrollback, which any attach client can snapshot; an agent's own files, its worktree and its shell are all still reachable. Denying `environ` while leaving those means a smaller published hole, not a separated agent.
- **A partial denial invites the wrong reading.** The failure this page is written against is someone citing the mechanism as more than it is. "Confined on Linux, when the profile is installed, against one of several routes" is exactly the kind of claim that gets quoted without its qualifiers.

None of that makes a profile worthless — a user who wants one is not wrong, and it does raise the cost of the specific `/proc` read. It is not a thing this project can claim, which is a different statement.

### What the token is still for, given all that

Nothing above makes it pointless, and the reasoning that it might is the reason this section exists.

- **It removes the *published* escalation.** Pane ids are advertised by `list-agents`, by `daemon status` and in the TUI. The token is not: it is absent from the `AgentRecord` clients receive and the daemon never logs it. So the #1077 path — read a pane id off the daemon's own status output and forge a message — no longer reaches anything, and an adversary needs a *different* capability, reading another process's environment — a deliberate act rather than a by-product of ordinary work.
- **It binds a message to one spawn**, which is the half that catches the accidental forgeries described above. That is a correctness property, not a security one, and it is the half that earns its keep on an ordinary day.

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

A refused `delegate`, `restart_role`, `spawn_role` or `list_targets` gets an `error` on the connection. A refused `get_seed` gets the same `{"seed":null}` as "nothing pending", so the pane's real occupant still receives its seed through the PTY-injection safety net. The log line names the verb, the claimed pane and the reason, and never the token.

**`work_done` and `dispatch` are told too, since issue #1129.** They have no response type of their own — their handlers produce nothing — so the daemon answers both with a `SignalAck` (`src/event.rs`) written at the gate, before the handler runs, and the CLI turns a refusal into a non-zero exit naming the reason. Until then they read no reply at all and exited 0 whatever the daemon did, which made a refused report indistinguishable from an acted-on one. The sender that matters here is not the adversary but the **legitimate** one: a `dot-agent-deck` binary in the pane older than the daemon forwards no token, is refused as `missing_token`, and used to report success on a completion that went nowhere. [#1182](https://github.com/vfarcic/dot-agent-deck/issues/1182) is that failure mode observed in the wild, where the silence also masked an unrelated bug underneath by sending readers to the daemon log's refusals instead of to the cause.

Three things the ack deliberately does **not** claim, because the whole point of writing it at the gate is that it costs the caller nothing to wait for:

- It is not a receipt for the work. `accepted: true` means the gate admitted the message, and the handler has not run yet. `dispatch`'s handler is awaited inline in the hook loop and spends a whole worktree-create-and-spawn; an ack written after it would park the calling agent for the duration.
- It does not make the *other* silent drops visible. A `dispatch` from a pane the registry holds no record for, and a `work_done` from a pane in no role map with no retained dispatch return, are both still logged and dropped with nothing said to the sender. Reporting those means answering after the handler, which is the trade above.
- It does not reach an **older** CLI, which does not read the line. That population is exactly the one the `missing_token` refusal is about, so the remedy stays what it was: update the binary the pane invokes, or set `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` on the daemon.

Telling the caller discloses nothing a caller on this socket could not already obtain. A refusal names why, and `missing_token`'s message says the claimed pane was issued a token — which tells a caller that guessed a pane id that the pane is real. Pane ids are already published by `list-agents`, by `daemon status` and in the TUI, and `delegate`, `restart_role`, `spawn_role` and `list_targets` have answered their refusals on this same socket since #1077, so any caller able to send one of those already had the same oracle. The refusal text is what keeps it no wider: it never echoes the presented token and never names the pane a token actually belongs to. The daemon does not publish the token anywhere else either — it is held on `RunningAgent`, not on the `AgentRecord` clients receive — but an agent that prints its own environment puts its token into its pane's scrollback, which any attach client can snapshot.

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
- `src/daemon.rs` `hook_provenance_*` — the real `run_hook_loop` over a real socket and registry: an attested `delegate` still routes, and a token-less one, a sibling's valid token and a foreign token are each refused without reaching the worker. Issue #1129 adds the same pair for `work_done`: an attested one is acknowledged *and* still reaches the orchestrator's PTY, a token-less one is refused on the connection with the `missing_token` code and reaches nothing.
- `src/event.rs` — that only the two fire-and-forget verbs are acknowledged at the gate (a second line on a verb that answers for itself would be read as its answer), that every verb answers a refusal, and that a line without the affirmative marker is not read as a refusal.
- `tests/hook_provenance.rs` — the real CLI forwards the token from its environment and omits the key when it has none, and (issue #1129) what it does with each of the four lines a daemon can send back: a refusal fails and names the reason, an admission is silent, a daemon that writes nothing is still a success, and a line that is not an ack is not read as a refusal.
- `orchestration/provenance/001` (`tests/e2e_hook_provenance.rs`, lane 1) — the whole chain through the real binary: a worker whose own command the daemon spawned reports twice, and a forgery sent between those two reports from outside the pane never reaches the orchestrator.

## What stays open

- **Same-uid `/proc/<pid>/environ` reading** is **not** open — it is decided, above: [The trust boundary](#the-trust-boundary-the-os-user). It is listed here so a reader arriving at this section does not go looking for a fix that is deliberately not coming. The half of [#1129](https://github.com/vfarcic/dot-agent-deck/issues/1129) that *was* code — a refused `work-done` or `dispatch` being invisible to the command that sent it — is closed by the `SignalAck` described above.
- **The silent drops the ack does not cover** — a `dispatch` from a pane with no registry record, and a `work_done` from a pane in no role map with no retained dispatch return. Both are logged and dropped with nothing said to the sender, for the reason given above (answering them means answering after the handler). Not tracked by its own issue; filed here so it is not mistaken for something #1129 closed.
- **Raw `AgentEvent` traffic** — hook events and `agent-event` — is not gated. It is a published schema that third-party producers emit, its producers are hook scripts already written into each agent's own configuration by existing installations, and `apply_event` auto-registers an unknown `SessionStart`. Issues [#543](https://github.com/vfarcic/dot-agent-deck/issues/543) and [#401](https://github.com/vfarcic/dot-agent-deck/issues/401) track it.
