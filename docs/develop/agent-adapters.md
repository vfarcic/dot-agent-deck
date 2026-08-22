# Agent adapters — adding a new agent

> **Developer / maintainer reference.** This page is the contract for adding a new agent to dot-agent-deck. It is intentionally excluded from the published documentation site and renders as plain Markdown here on GitHub.

dot-agent-deck is a control plane over **external** agent processes: it spawns them, observes their status, and coordinates them. It does not run an agent loop of its own. "Adding an agent" therefore means teaching the deck two things — how to *recognise* the agent, and how the agent's activity *reaches* the deck as [`AgentEvent`](../../src/event.rs)s — and then rendering the result. This guide documents the seams that carry those two things and walks the whole change end to end, using **Codex** (the wrapper-strategy agent shipped by [PRD #20](../../prds/20-multi-agent-support.md)) as the worked example.

## Design philosophy: a curated registry, not a plugin system

The agent set is a **curated, compiled-in registry** ([`src/agent_registry.rs`](../../src/agent_registry.rs)) plus a **small finite set of integration strategies**. Two deliberate consequences follow:

- **Runtime / user extensibility is an explicit non-goal.** There is no config knob, no `agent_type` free-form string, and no drop-in plugin directory that lets an end user add an agent without a code change. Every change to dot-agent-deck ships in a release anyway, so requiring a recompile to add an agent costs nothing we weren't already paying — and it buys a typed identity ([`AgentType`](../../src/event.rs)) that the compiler can check everywhere.
- **Adding an agent is centralisation, not destructuring.** Before the registry, each agent's data — its label, its detection pattern, its default command, its colour, which install mechanism it used — was scattered across `match AgentType` arms in `src/event.rs`, `src/ui.rs`, and the install/materialize modules. The registry pulls all of that into **one cohesive [`AgentSpec`] entry per agent**. The identity stays typed; the win is that the per-agent facts live in exactly one place.

So the two shapes of "add an agent" are:

- **Reuse a shipped strategy** → a registry entry (+ any strategy-specific *data*, e.g. a wrapper rule set) + a release. This is the cheap, common path. Codex, Gemini, and any other stdout-emitting CLI are this.
- **Introduce a genuinely new mechanism** → implement a new [`IntegrationStrategy`] **once**, then it is a registry entry thereafter. Aider's log-watcher is this: the *first* log-watcher agent pays for the strategy; the second is config.

## The four shipped integration strategies

Events reach the deck by different mechanisms per agent — that is why this layer is inherently *code*, not data. There are four shipped mechanisms, each named by one [`IntegrationStrategy`] variant, each with one shipped agent as its reference implementation:

| Strategy | Reference agent | Mechanism | Install / activation | Module |
|---|---|---|---|---|
| `NativeHooks` | Claude Code, Devin | Hook scripts installed into the agent's own config that shell back to the deck | Machine-global, at TUI startup (`auto_install`) | [`src/hooks_manage.rs`](../../src/hooks_manage.rs), [`src/devin_hooks_manage.rs`](../../src/devin_hooks_manage.rs) |
| `Plugin` | OpenCode | A JS plugin materialized into the agent's plugin directory | Machine-global, at TUI startup (`auto_install`) | [`src/opencode_manage.rs`](../../src/opencode_manage.rs) |
| `Extension` | Pi | A bundled TypeScript extension materialized into the agent's HOME (`include_str!`) | Auto-materialized at spawn time (guarded on the spawn command being `pi`) | [`src/orchestrator_ext.rs`](../../src/orchestrator_ext.rs) — see [pi-extension.md](pi-extension.md) |
| `Wrapper` | Codex | `dot-agent-deck wrap -- <cmd>` spawns the agent, passes stdio through transparently, and tees stdout/stderr through pattern detection into events | No install step — the launch command is rewritten to wrap the agent | [`src/wrap.rs`](../../src/wrap.rs) |

Every one of these is *just another `AgentEvent` producer*. None of them invents a parallel status channel — they all serialize into the same [`AgentEvent`](../../src/event.rs) stream the daemon already consumes and re-broadcasts to attached TUIs. The strategy only determines *how the bytes get produced*, never *what the wire looks like*.

The startup installer dispatches on the strategy from the registry rather than from a hardcoded list ([`src/main.rs`](../../src/main.rs)): it iterates `agent_registry::ALL` and runs the appropriate `auto_install` for `NativeHooks` and `Plugin`. `Extension` materializes at spawn time instead, and `Wrapper` has no install step at all, so both are skipped there — but they are still *registry entries*, so detection, badges, filtering, and default commands come from the same place as every other agent.

### Why Codex is the worked example

Codex is the first **wrapper** agent, and a wrapped session is the first place the `live_target` / `send_result` distinction actually bites (below). It exercises every seam a "reuse a shipped strategy" agent touches — a new `AgentType`, a registry entry, a wrapper rule set, a live-target declaration, a badge colour, and the full test ladder — without needing a brand-new mechanism. If you can follow the Codex change, you can add Gemini by analogy in an afternoon.

## The `AgentEvent` contract (what every strategy produces)

[`AgentEvent`](../../src/event.rs) is a **stable public API**: third parties author events against it, so fields are added *additively* (optional + `#[serde(skip_serializing_if)]` so old and new payloads round-trip unchanged) and never repurposed. The record carries `session_id`, `agent_type`, `event_type`, optional tool/prompt/cwd detail, routing ids (`pane_id`, `agent_id`), and — added by PRD #20 — `agent_version`, `schema_version` ([`AGENT_EVENT_SCHEMA_VERSION`]), and `live_target`.

The event schema version ([`AGENT_EVENT_SCHEMA_VERSION`], currently `1`) versions the **payload shape of a single record**. It is **distinct** from [`crate::daemon_protocol::PROTOCOL_VERSION`], which versions the **attach-socket handshake** between the TUI and the daemon. The two move independently; do not conflate them. Adding an agent that rides the existing `AgentEvent` wire (every shipped strategy does) touches neither version — see [versioning.md](versioning.md) and the cross-version check in [CLAUDE.md rule 12](../../CLAUDE.md).

## Step-by-step: adding an agent

The steps below are keyed to the real seams. Codex is used throughout; a **reuse-a-strategy** agent does steps 1, 2, (3), 4, 5, 6 and *no* new mechanism; a **new-mechanism** agent additionally implements one new strategy (see step 3's second half).

### 1. Add the `AgentType` variant — `src/event.rs`

Add your agent to the [`AgentType`] enum. It serializes `snake_case`, so `Codex` becomes `"codex"` on the wire.

```rust
pub enum AgentType {
    ClaudeCode,
    OpenCode,
    Pi,
    Codex,   // ← new
    #[serde(other)]
    None,
}
```

Leave the `#[serde(other)]` catch-all on `None`: it is the forward-compatibility guard that makes an unrecognized wire value (a newer agent reaching an older reader) decode to the neutral "No agent" placeholder instead of failing the whole-record decode. You do **not** touch `from_command` — it delegates the basename→type lookup to the registry (step 2), so the recognized set updates automatically.

### 2. Add the registry entry — `src/agent_registry.rs`

This is the heart of the change: one [`AgentSpec`] with every per-agent fact.

```rust
pub static CODEX: AgentSpec = AgentSpec {
    agent_type: AgentType::Codex,
    label: "Codex",                       // shown in card titles / Display
    detect_basenames: &["codex"],         // `codex …` → AgentType::Codex
    default_command: Some("codex"),       // the canonical launch command
    strategy: Some(IntegrationStrategy::Wrapper),
    badge_color: Color::LightYellow,      // step 5 — named ANSI colour only
};
```

Then add it to the `ALL` slice so detection, startup dispatch, badges, and the type filter all pick it up:

```rust
pub static ALL: &[&AgentSpec] = &[&CLAUDE_CODE, &OPEN_CODE, &PI, &CODEX];
```

…and add its arm to `spec()` so the lookup stays total over every variant. That is the whole registry change. Detection (`detect_from_basename`), the `Display` label (`src/ui.rs` reads `spec(self).label`), the default command, the badge colour, and the `type:` filter alias (`resolve_type_alias`) are now *derived* from this one entry — there are no other sites to edit for those.

### 3. Wire the integration strategy

**If you are reusing the `Wrapper` strategy (the cheap path — Codex, Gemini):** you write *no new mechanism*, only *data*. Add a [`RuleSet`] in [`src/wrap.rs`](../../src/wrap.rs) and select it by agent type in `ruleset_for`:

```rust
pub static CODEX: RuleSet = RuleSet {
    // `codex exec --json` emits one compact JSON object per line (JSONL); key
    // card state off the record's `type` discriminator rather than guessing
    // from free text. Matching the quoted discriminator keeps an incidental
    // "error" inside reasoning/command text from flipping the card.
    error_markers: &["\"type\":\"error\""],
    idle_markers: &["\"type\":\"turn.completed\""],
};

fn ruleset_for(agent_type: &AgentType) -> &'static RuleSet {
    match agent_type {
        AgentType::Codex => &CODEX,
        _ => &GENERIC,       // any non-blank line = working; a few markers = error
    }
}
```

The wrapper runtime (`run_wrap`, `tee`, `Detector`) does not change — the `Detector` debounces a stream of classifications into one event per state change, and it is driven by whichever `RuleSet` `ruleset_for` returns. The `GENERIC` fallback (any non-blank line is activity, a handful of substrings flip to error, idleness comes from process-exit quiescence) already makes `wrap -- <arbitrary-command>` do something useful, so a per-agent rule set is an *upgrade*, not a prerequisite.

#### Codex is a **hybrid**: native hooks under the wrapper (PRD #20 W1)

Stdout scraping cannot reach full parity for *interactive* Codex — bare `codex` paints an ANSI TUI on stdout with no JSON, so the coarse `CODEX` `RuleSet` above can only ever see a wall of redraw text (it never reliably reaches `Idle`/`Error` mid-session and emits no tool/prompt detail). Codex 0.144.4, however, ships a **Claude-Code-compatible native hooks engine**. So Codex keeps `IntegrationStrategy::Wrapper` as its **PTY host + hook injector**, but its rich events come from **native hooks**, not the classifier — the `CODEX` `RuleSet` above is retained only as a coarse fallback.

Concretely, when the deck starts (and again whenever the wrapper launches a real `codex`):

1. It installs a `hooks.json` into the active `CODEX_HOME` ([`src/codex_hooks_manage.rs`](../../src/codex_hooks_manage.rs)) whose every command hook shells `dot-agent-deck hook --agent codex`. Those hook payloads are the **same shape Claude posts**, so they are ingested by the existing [`src/hook.rs`](../../src/hook.rs) `handle_hook` `"codex"` arm (stamping `AgentType::Codex`) — no new wire, no `PROTOCOL_VERSION` bump. The installed rule set covers the lifecycle (`SessionStart`/`Stop`), the prompt (`UserPromptSubmit`), tools (`PreToolUse`/`PostToolUse`), permission, compaction, and subagent boundaries — the same class Claude delivers. The installer is **safe for the user's real `~/.codex`**: it resolves `CODEX_HOME` the way Codex does (`$CODEX_HOME`, else `$HOME/.codex` — the user's REAL home in production, never a throwaway), **merges** (a user's own hooks and `config.toml` are never clobbered), identifies its OWN entries by the **exact command signature** `… hook --agent codex` (so a user hook that merely mentions `dot-agent-deck` is preserved), writes **atomically** (temp file + `rename(2)`, serialized by an in-process lock, so a crash or two concurrent Codex spawns can't corrupt the file), and **never discards** existing content it can't parse (malformed JSON is backed up to `hooks.json.bak` and the install errors; a structurally-incompatible shape errors without touching the file).
2. It records **scoped, per-hook, hash-pinned trust** for exactly the entries it just authored (PRD #20 §4.1). Codex requires non-managed command hooks to be *trusted* before they run (an interactive `/hooks` review otherwise), and it keeps that trust in `$CODEX_HOME/config.toml` as `[hooks.state."<sourcePath>:<event_snake>:<group_idx>:<handler_idx>"] { enabled, trusted_hash }`. The deck asks Codex itself for the identity of each hook — `codex app-server` (stdio JSON-RPC) → `hooks/list { cwd }` returns every hook's `key`, `currentHash`, `trustStatus`, `sourcePath`, `command`, and `isManaged`, with no credentials and no network — and then writes a trust record **only** for entries that pass all three parts of the security predicate (`deck_owned_entries`): the `sourcePath` is the pinned home's own `hooks.json`, the command carries the exact deck signature `… hook --agent codex`, and `isManaged` is false. Everything else — a foreign command in the *same* `hooks.json`, a deck-shaped command sourced from a different home, a managed hook, a user command that merely mentions `dot-agent-deck` — stays untrusted. The `config.toml` edit is **format-preserving** (`toml_edit`: only the `hooks.state."<key>"` tables are inserted/replaced), atomic (temp + `rename(2)`), and idempotent, so the user's comments, `model = …`, auth references, and their own trust records survive byte-intact.

   There is deliberately **no `--dangerously-bypass-hook-trust`** any more (it was removed in PRD #20 §4.1 in response to Greptile's P1). That flag was *invocation-global* — it trusted every enabled hook in the active `CODEX_HOME` — and, being argv, any launcher on `PATH` could receive it, re-point `CODEX_HOME`, and have Codex trust hooks the deck never inspected. Scoped trust closes that class **by deletion**: nothing trust-related reaches argv, so there is nothing to hijack or forward. It is also strictly *narrower* and *launch-method agnostic*: trust lives in the home, so bare `codex`, `/abs/path/codex`, `./run_codex.sh`, and `devbox run codex-big` behave identically, and a user with third-party hooks is no longer degraded (the old code refused to bypass at all in that case). Trust is content-pinned, so any edit to a trusted definition flips it to `modified` and Codex refuses to run it — every failure mode (Codex absent, `hooks/list` shape drift, a stale hash after a Codex upgrade) ends in hooks *not firing* and events degrading to the coarse stdout classifier, never in silent over-trust.

   Residuals, both fail-closed: (a) the trust write covers `CODEX_HOME/hooks.json` only — project-local `<repo>/.codex`, plugin, and `config.toml`-defined hooks are not inspected or trusted (trust those once via Codex's `/hooks` review if you want them); (b) the deck **pins** `CODEX_HOME` on the child, but a launcher script can still re-export it before exec'ing codex, and a child controls its own environment. After the bypass removal that is only a *functionality* loss — the re-homed Codex has neither the deck's `hooks.json` nor its trust records, so no events arrive — not a trust leak. (c) `hooks/list` is an `[experimental]` app-server surface; if its shape moves, the trust step degrades quietly rather than blocking a spawn. Do **not** "harden" this by hand-computing the SHA-256 — that guesses Codex's canonicalization; recording the hash Codex itself reports is what keeps trust honest.

##### The live hook payload shape (what interactive Codex actually posts)

Codex 0.144.4's native hooks post the **Claude-Code JSON shape**, and — verified against a live interactive turn (the `tests/e2e_codex_hooks.rs` real-agent test, aligned to the live payload) — a shell tool call arrives with **`tool_name: "Bash"`** and a **plain-string `command`**, exactly like Claude:

```json
{
  "session_id": "…",
  "hook_event_name": "PreToolUse",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_use_id": "…",
  "tool_input": { "command": "touch sentinel.txt" }
}
```

So the `UserPromptSubmit` prompt text, the `Bash` tool name, and the command detail all reach the card through the *same* `extract_tool_detail` `"Bash"` arm the Claude path uses — no Codex-specific parsing required for the common case. `hook.rs` **also** carries defensive `"shell"` (argv-array `command`) and `"apply_patch"` (patch-envelope file path) arms; these tolerate the alternative shape that the `codex exec --json` stream / older Codex builds can emit, but the shipped interactive hook path does not exercise them. Treat `Bash` + string as the canonical shape and the argv/patch arms as graceful fallbacks.

##### Launcher/wrapper scripts need nothing (PRD #20 §4.1/§4.2.1)

Because trust is recorded in the Codex **home** rather than injected into **argv**, how Codex is launched no longer matters. `devbox run codex-big`, a `run_codex_agent.sh`, an alias, a custom absolute path whose basename isn't `codex` — all get the same hooks and the same trust as bare `codex`, and none of them needs a single line added:

```sh
#!/bin/sh
# run_codex_agent.sh — nothing deck-specific belongs in here.
exec codex "$@"
```

Two mechanisms make that true:

- The deck installs its `hooks.json` **and** records scoped trust **once at startup** (`codex_hooks_manage::auto_install_and_trust_at_startup`, wired as Codex's `startup_auto_install` and from the `daemon serve` entry), self-guarded on `codex` being on `PATH` and a resolvable home. This is deliberately **command-agnostic**, following PRD #201's Pi precedent: the spawn seam can only recognize a `codex` basename, so a launcher used to get *no* integration at all. Hook events reach the right card through the `DOT_AGENT_DECK_PANE_ID` the hook child inherits — not through the wrapper — so status/prompt/tool detail arrive even for a pane the deck never wrapped.
- The wrapper repeats install + trust just before it spawns a Codex-identity child (bare `codex`, an absolute path, or a launcher inside a deck-spawned pane), so a home that changed since startup is refreshed. It still pins the resolved `CODEX_HOME` on the child.

Note `dot-agent-deck wrap --agent claude -- codex` installs no Codex hooks and records no trust — the Codex path is gated on Codex *identity*, not on the program name. And the deck never resolves or verifies the executable: with the bypass gone there is nothing dangerous to hand out, so no launch form has to be rejected for being unrecognizable.

If Codex still won't run the deck's hooks, the usual cause is a launcher that re-exports `CODEX_HOME` (the deck's pin only reaches the child it spawns) — drop that re-export, or trust the deck's hooks once through Codex's interactive `/hooks` review in that home. Until then the card falls back to the coarse stdout classifier: degraded status, no tool/prompt detail.

#### The readiness contract every Wrapper adapter must satisfy (PRD #225)

This is the trap the next wrapper adapter would otherwise walk straight into, so read it before you emit your first event.

`dot-agent-deck wrap` emits an `EventType::SessionStart` the instant `cmd.spawn()` returns — see `Emitter::emit_fork_session_start` in [`src/wrap.rs`](../../src/wrap.rs). That event exists for **one** reason: to surface the dashboard card immediately, so a slow-booting agent is not invisible for several seconds. It is emitted at *fork* time, when the child is typically still just the launcher (`devbox`, a shell, `node` starting up) and the agent's own TUI does not exist yet. It therefore does **not** mean "this agent can accept input."

Because the deck also uses `SessionStart` as a **readiness gate** — `wait_for_session_start` in [`src/state.rs`](../../src/state.rs), which `dispatch_one_owned` waits on before injecting a `clear = true` delegate's prompt, and which `crate::spawn::spawn` reuses for a scheduled card's prompt — those two meanings have to be told apart. The contract:

1. **The wrapper's fork-time `SessionStart` MUST carry the origin marker.** `metadata["session_start_origin"] = "wrapper_fork"` — write it through the shared constants `SESSION_START_ORIGIN_METADATA_KEY` / `WRAPPER_FORK_SESSION_START_ORIGIN` in [`src/event.rs`](../../src/event.rs), and read it back with `AgentEvent::is_wrapper_fork_session_start()`. Every other producer (native hooks, a log watcher, an SDK adapter) omits the key, and an absent key means "this `SessionStart` came from an initialized session."
2. **The gate skips a marked event only for agents that will emit a real one.** `session_start_means_ready` treats a marked event as *not ready* **iff** the agent's registry spec has a native-hook installer (`agent_registry::spec(&agent_type).hook_install.is_some()`). The discriminator is a registry property, not an `== Codex` special case, so a new adapter inherits the right behaviour from its registry entry alone.

The two failure modes this balances, both silent:

- **Marked event, native hooks present, no native `SessionStart` ever emitted → the delegate starves.** The gate correctly ignores the fork-time event, waits out `SESSION_START_WAIT_TIMEOUT` (30 s, sized from measured Codex boot rather than the inherited Claude-era 10 s), then writes the prompt blind. So set `hook_install` **only** if your agent genuinely posts a `SessionStart` hook once its session is live and interactive — that field is what the gate reads as the promise "a real readiness signal is still coming."
- **Unmarked fork-time event on a hooked agent → the prompt is destroyed.** This is the original defect. The gate releases at fork time, the deck writes the prompt plus a CR into a PTY where only the launcher is running, and the line discipline (canonical mode, echo on) echoes the text back and swallows it. The agent then boots, clears the alternate screen, and sits at an empty composer — the operator sees a worker that "restarted and did nothing," and the *echo* is why they also report having seen the prompt arrive.

The inverse case is just as load-bearing: **a pure-Wrapper agent with no native hooks (`hook_install: None` — Gemini, PRD #211) must still emit its fork-time event, and must rely on it as its sole readiness signal.** Skipping marked events unconditionally would regress every such agent to a full 30 s timeout on every delegate. So do not "fix" a hookless adapter by suppressing its fork-time event, and do not give it a `hook_install` it cannot honour.

**Measured on 2026-07-27 against codex-cli 0.145.0: Codex does not post its native `SessionStart` when its TUI comes up — it posts it when the first *turn* starts.** A real `clear = true` delegate to a wrapped Codex worker, observed on the daemon's event broadcast: the wrapper's marked fork-time `SessionStart` at T+0, then the unmarked native `SessionStart` at **T+29.999 s** — 5 ms before the `UserPromptSubmit` that the gate's own injected prompt produced. So for Codex the fallback is the *healthy* path, not the exception: the gate correctly refuses the marked event, waits out `SESSION_START_WAIT_TIMEOUT`, writes the prompt into a long-booted (and therefore live) TUI, and the worker does the work. Two consequences worth knowing before you touch this code. First, every Codex delegate costs ~30 s of latency, and the only way to shorten it is a readiness signal that exists *before* a prompt (a wrapper-side "TUI is up" detection, say) — not a shorter timeout. Second, a test cannot wait for a genuine pre-prompt Codex `SessionStart` as a precondition for delegating: that is circular, since the event it waits for can only be caused by the prompt it is gating.

The scheduler's mirror of that wait can be shortened per-run with `DOT_AGENT_DECK_SESSION_START_WAIT_MS` (milliseconds), which the e2e harness pins so a no-hook fallback does not cost the full production wait. It is **clamped** to `[100 ms, SESSION_START_WAIT_TIMEOUT]` with a `warn!` when an out-of-range value is clamped: `0` would turn the gate back into the unsynchronized write this whole section exists to prevent, and an absurd value would hang delivery with no output to explain it. So do not reach for it to "make readiness work" for a new adapter — if your agent needs the gate tuned, the answer is a correct readiness signal, not a shorter wait.

Wire compatibility is additive in both directions and is a **semantic no-op**, not a [`PROTOCOL_VERSION`](versioning.md) bump: an old wrapper sends no marker and a new daemon treats its event exactly as it does today (racy, no worse than before), and a new wrapper's marker is ignored by an old daemon. Rule 12's cross-version run still applies because the change touches the daemon and hooks.

#### The launch-shape invariant (PRD #225)

Wrapping is the one thing that rewrites a pane's exec line, so a Wrapper adapter also inherits this rule. Two fields on `RunningAgent` ([`src/agent_pty.rs`](../../src/agent_pty.rs)) carry an agent identity and they are **not** interchangeable: `agent_type` is the OBSERVED display badge (upgraded in place by `set_agent_type` when a hook event reveals the real agent), while `spawn_agent_type` is the identity that drove the launch. `set_agent_type` writes only the badge. That split exists because a value recorded for display used to leak into the exec line: a `devbox run codex-big` pane spawned unwrapped, Codex's hooks taught the registry `Some(Codex)`, and the pane's first `clear = true` delegate brought it back up *wrapped* — the same pane running a different process tree before and after.

On respawn the rule is: **the wrap decision is derived from the command actually being launched; the pane's spawn-time identity only fills in for a command that implies no agent type.** `respawn_agent_for_pane` receives the CURRENT role command (the daemon re-reads `.dot-agent-deck.toml` at delegate time), so an edited command is honored — and the wrap decision follows that edit rather than contradicting it. Replaying a frozen `Some(Codex)` verbatim would relaunch an edited `claude` command as `dot-agent-deck wrap --agent codex -- claude`; dropping the frozen identity instead would flip an initially-wrapped `devbox run codex-big` pane to bare. Deriving first, with the frozen identity as the fallback, is the only rule that avoids both — and it makes an explicit creation-time identity and an inferred one behave the same way. The residual limit is documented rather than papered over: if the command implies nothing *and* its underlying agent changed (`devbox run codex-big` → `devbox run claude-big`), the pane keeps its creation-time identity and has to be recreated.

Coverage to mirror when you add a wrapper agent: `orchestration/delegate/007` (a marked fork-time event must *not* release a hooked agent), `orchestration/delegate/008` (a marked fork-time event *must* release a hookless one, well inside the fallback), `codex/spawn/007` (a hook-learned badge must never mutate the exec line across respawn), `codex/spawn/008` (a respawn's wrap decision follows the command being launched, for both an unchanged and an edited role command), and the real-agent `orchestration/delegate/009` (a `clear = true` delegate to a real wrapped Codex worker delivers the prompt and the worker acts on it).

**If you need a genuinely new mechanism (e.g. Aider's log-watcher):** implement a new [`IntegrationStrategy`] variant **once** — a new module that produces `AgentEvent`s (e.g. `dot-agent-deck watch --agent aider --log <path>` tailing a structured log and parsing entries) plus its dispatch. Note that today's `Commands::Watch` is an **unrelated generic interval-runner**, not a log watcher; a log-watcher strategy is a separate command. After the strategy exists once, the *second* agent that uses it is back on the cheap path (a registry entry naming the strategy).

### 4. Declare `live_target` / writability — `src/event.rs`, and your producer

A dashboard-visible session is **not** necessarily a live, writable target. Native PTY panes (Claude / OpenCode / Pi) are `Live`: the daemon owns the PTY and can inject input. A **wrapper** session's writability depends on *how* it was launched, so each producer declares a per-session [`LiveTarget`] descriptor with two axes:

- `kind` ([`TargetKind`]): the concrete handle — `process | pty | tmux | sdk | none`.
- `writable` ([`Writable`]): what can be done with it now — `live` | `history-only` | `none`.

The Codex wrapper decides this **per invocation** (see `run_wrap` in [`src/wrap.rs`](../../src/wrap.rs)):

- **Inside a deck-managed pane** (`DOT_AGENT_DECK_PANE_ID` set) — the common case for a deck-spawned Codex pane — the child runs on a daemon-backed PTY, so the wrapper stamps `{ kind: Pty, writable: Live }`: the daemon's dashboard writes reach the child through the pane PTY → the wrapper's stdin → the inner PTY.
- **A standalone `wrap`** (no pane) has no deck-controlled write handle — the child's terminal is the user's own — so it stamps `{ kind: Process, writable: HistoryOnly }`, and the UI renders the card view-only rather than inviting input it can't deliver.

Native PTY panes leave `live_target` unset (`None`), which the UI reads as the historical live/writable default. Declare the descriptor honestly so a session that *can't* take input never presents a live input affordance.

When the dashboard *does* deliver input, the send path returns an honest [`SendResult`] instead of fire-and-forget: `applied`, `queued`, `stale`, `wrong-session`, `history-only`, or `no-live-target`. A `history-only` / `stale` / `wrong-session` result surfaces feedback rather than silently dropping the keystroke. (Proving *consumption* of a specific input — generation counters, output-cursor diffing — is explicitly out of scope; the lightweight `live_target` + `send_result` model is enough.)

### 5. Badge colour + what comes for free

Set `badge_color` on the registry entry to a **named ANSI colour** (e.g. `Color::LightYellow`) — never an absolute `Color::Rgb`, so terminal themes can remap it, matching the palette policy in [`src/palette.rs`](../../src/palette.rs). Pick one not already used by another agent (Claude `LightMagenta`, OpenCode `LightGreen`, Pi `LightCyan`, Codex `LightYellow`), and never the neutral `DarkGray` reserved for the "No agent" placeholder.

Because the card renderer reads `agent_registry::spec(&session.agent_type).badge_color` and the label from the same entry, the coloured type badge appears with **no `src/ui.rs` change**. Two more things also come for free from the registry:

- **The `type:` filter.** The `/` search parses `type:<alias>` tokens and resolves them through `resolve_type_alias`, which matches case-insensitively against every entry's label *or* any detection basename. So `type:codex` works the moment the registry entry exists — no filter code to touch.
- **New-agent default-command wrapping.** At the TUI new-agent spawn seam, `wrap::wrap_launch_command` rewrites a bare command into `dot-agent-deck wrap --agent <basename> -- <command>` **iff** the resolved agent's strategy is `Wrapper` (idempotent, so a restore never double-wraps). A Wrapper-strategy agent is therefore launched under the wrapper automatically, driven entirely by the registry `strategy` field.

### 6. Tests + the behaviour-preserving constraint

Adding an agent is only "done" when it is covered at every layer the shipped agents are. Mirror the Codex test set:

- **Fast-tier unit tests** for the registry identity and detection — that the new type resolves from its basename, the `AgentSpec` fields are what you expect, and the strategy is correct. See [`tests/codex_adapter.rs`](../../tests/codex_adapter.rs) (`codex_detect_001_registry_identity_is_complete`).
- **Wrapper `RuleSet` classification tests** (if reusing the wrapper) — that realistic agent output lines map to the right `DetectedEvent`. See the JSONL cases in `codex_adapter.rs` (`codex_wrap_001_jsonl_output_maps_to_dashboard_states`) and the pure-function tests in `src/wrap.rs`.
- **A synthetic e2e** (`e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`) — a PTY-attached test driving a deterministic stand-in that emits realistic agent output, asserting the event stream *and* the visible dashboard card. See `codex_wrap_001_synthetic_jsonl_reaches_dashboard` in [`tests/e2e_codex_wrapper.rs`](../../tests/e2e_codex_wrapper.rs).
- **A real-agent e2e** — the same PTY-attached shape, but driving the *real* agent on a **cheap model** through a cheap, deterministic-enough operation (list a directory and report a uniquely-named fixture **sentinel file**, so the assertion survives LLM phrasing variance). See `codex_live_001_real_model_lists_sentinel_in_wrapped_pane`. Real-agent tests live in the pre-PR e2e tier (flaky-tolerant, never in CI) — [CLAUDE.md rule 4](../../CLAUDE.md) is the bar: **at least one test per major feature must validate it as a user actually uses and sees it.**
- **A skip harness** — add a `check_<agent>_available` helper (and credential import if the agent needs auth) to [`tests/common/mod.rs`](../../tests/common/mod.rs), modelled on `check_codex_available`, so a missing/unauthenticated CLI cleanly *skips* the real-agent test rather than failing it. Keep the model the gate probes and the model the tests launch the SAME value (for Codex, `common::codex_test_model()`), or the suite can skip for a reason the scenario would not have hit — a skip reads as a pass, so a wrong gate is silent no-coverage. That helper's default (`gpt-5.1-codex-mini`) is reachable only with **ChatGPT-subscription** Codex credentials; a host whose `~/.codex/auth.json` is an **API key** gets `404 Not Found: Model not found` from `/v1/responses` for the whole `codex-*` family, so on such a host export `DOT_AGENT_DECK_CODEX_TEST_MODEL=gpt-5-nano` (or any cheap model the key can reach) to run the real-Codex tests.

**The behaviour-preserving constraint.** For the *existing* agents, the registry move (and any refactor along the way) must be **behaviour-preserving**: the existing test suite must pass **unchanged**. Do not edit an existing test to make it green — if it needs editing, the change altered observable behaviour and that is a bug, not a test update. New coverage for your agent is *additive* on top of the untouched existing suite. Run `cargo fmt --check`, `cargo clippy --all-targets --features e2e -- -D warnings`, and `cargo test-fast` per task, and `cargo test-e2e` before the PR. The clippy flags matter for an adapter in particular: your adapter's e2e coverage lives in `tests/e2e_*.rs`, and without `--features e2e` those files compile to empty crates — clippy would report clean over the very code you just wrote (issue #407).

## Follow-up agents built on this seam

Two follow-up PRDs build directly on the PRD #20 machinery documented here:

- **Gemini** — a **wrapper**-strategy agent, so a thin registry entry + a Gemini-specific `classify_line` rule set + detection + e2e. It reuses `dot-agent-deck wrap` wholesale; the PRD is small *because the wrapper strategy already exists*.
- **Aider** — introduces the **new log-watcher** strategy (`dot-agent-deck watch --agent aider --log <path>` tailing Aider's structured logs into `AgentEvent`s). That PRD carries the one-time log-watcher `IntegrationStrategy` implementation; every log-watching agent after it is back on the cheap path.

### Devin — the second `NativeHooks` agent

Devin CLI is the second agent to reuse the `NativeHooks` strategy, and it is the cheapest possible registry addition: a typed identity, a registry entry, and one `hook.rs` ingestion arm. No wrapper, no trust ceremony, no plugin. Its hooks post the **same Claude-Code JSON shape** Codex's do, so the existing `ClaudeCodeHookInput` deserializer and `build_event_typed` path are reused wholesale — only the `AgentType` stamp differs.

The one place Devin diverges from Claude's installer is **config safety**. Claude's `~/.claude/settings.json` is a file the deck has always rewritten wholesale, so `hooks_manage` treats any read/parse failure as an empty config. Devin's `~/.config/devin/config.json` is a **shared** file holding the user's model, permissions, MCP servers, and theme — and Devin documents it as JSON *with comment support*, which `serde_json` cannot parse. Claude's parse-failure-as-empty fallback would silently destroy a valid user config the first time anyone wrote a `//` comment in it. So `devin_hooks_manage` borrows `codex_hooks_manage`'s discipline instead: only `NotFound` is empty; malformed content is backed up to `config.json.bak` and the install errors; a structurally-incompatible shape errors without touching the file; the read-modify-write is serialized by an in-process mutex and published atomically.

**The publish itself is shared — write the third adapter against it, not against a copy.** `codex_hooks_manage` and `devin_hooks_manage` are the same installer twice, and the copy is how a defect outlived its fix: `write_atomic` published through `File::create`, which applies `0666 & !umask` (0644 under a typical 022 umask, **0664 — group-writable — under 002**), and the `rename` then replaced the destination with that wider file. Devin's config is shipped at 0600 and holds `devin.org_id`, so #360 fixed it there by carrying the destination's own mode over (falling back to 0600 when the file is new); the Codex copy kept the bug for another release, over `hooks.json` *and* the user's `config.toml`, until #382. Both now call [`agent_hook_config`], which holds that publish and the `build_command` quoting once. `hooks_manage` deliberately keeps its own `write_atomic`: the Claude adapter publishes through `create_new` so a leftover temp file cannot be a symlink the write follows out of the directory (#534), and that hardening has not been reviewed for the other two. A new `NativeHooks` adapter should call the shared helper rather than copy either.

Devin also resolves its config directory the standard XDG way — `$XDG_CONFIG_HOME/devin` when that variable is set, `~/.config/devin` otherwise — so `devin_config_dir` must too. Writing to the literal `~/.config/devin` on a machine that sets `XDG_CONFIG_HOME` installs hooks into a file Devin never opens: the install reports success and no event ever arrives. Off Unix the resolver returns `None` and every caller degrades to a documented skip, the same call `codex_home` makes and for the same reason.

**A non-interaction worth recording — Devin's Claude import.** Devin documents that it reads hooks from Claude's files (`~/.claude/settings.json`, `~/.claude.json`) when `read_config_from.claude` is enabled, which is the default — exactly where the deck installs its Claude hooks. On paper that predicts two hook invocations per lifecycle event from one Devin session, one stamped `AgentType::Devin` and one `AgentType::ClaudeCode`. Measured against devin 3000.3.27 it does not happen: with both hook sets installed and the import left at its default, a real session emits exactly one event per step, all stamped `devin`, in print mode and in an interactive pane alike. An earlier revision detected this "conflict" and advised setting `"read_config_from": { "claude": false }`; that was dropped because it fired on the deck's own default configuration and would have had users disable their Claude rules, skills, commands and MCP imports to fix a symptom that never occurred. If duplicates are ever observed in the wild, add the detection back then — and note that a launcher-started pane (`devbox run devin-big`, whose basename does not resolve to Devin) would latch to whichever agent type the first event carries, since `state.rs` only adopts a session's agent type while it is still `None`.

[`agent_hook_config`]: ../../src/agent_hook_config.rs
[`AgentSpec`]: ../../src/agent_registry.rs
[`AgentType`]: ../../src/event.rs
[`IntegrationStrategy`]: ../../src/agent_registry.rs
[`RuleSet`]: ../../src/wrap.rs
[`LiveTarget`]: ../../src/event.rs
[`TargetKind`]: ../../src/event.rs
[`Writable`]: ../../src/event.rs
[`SendResult`]: ../../src/event.rs
[`AGENT_EVENT_SCHEMA_VERSION`]: ../../src/event.rs
