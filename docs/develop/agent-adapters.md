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
| `NativeHooks` | Claude Code | Hook scripts installed into the agent's own config that shell back to the deck | Machine-global, at TUI startup (`auto_install`) | [`src/hooks_manage.rs`](../../src/hooks_manage.rs) |
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

Concretely, when the wrapper launches a real `codex`:

1. It installs a `hooks.json` into the active `CODEX_HOME` ([`src/codex_hooks_manage.rs`](../../src/codex_hooks_manage.rs)) whose every command hook shells `dot-agent-deck hook --agent codex`. Those hook payloads are the **same shape Claude posts**, so they are ingested by the existing [`src/hook.rs`](../../src/hook.rs) `handle_hook` `"codex"` arm (stamping `AgentType::Codex`) — no new wire, no `PROTOCOL_VERSION` bump. The installed rule set covers the lifecycle (`SessionStart`/`Stop`), the prompt (`UserPromptSubmit`), tools (`PreToolUse`/`PostToolUse`), permission, compaction, and subagent boundaries — the same class Claude delivers. The installer is idempotent, merges (it never clobbers a user's own hooks or `config.toml`), and resolves `CODEX_HOME` the way Codex does (`$CODEX_HOME`, else `$HOME/.codex` — the user's REAL home in production, never a throwaway).
2. It launches `codex` with `--dangerously-bypass-hook-trust`. Codex requires command hooks to be *trusted* before they run (an interactive `/hooks` review otherwise); because the deck **authors its own hook definition**, it vets the source — itself — and bypasses that prompt for this deck-controlled spawn.

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

##### Trust flag and launcher/wrapper scripts (important)

The `--dangerously-bypass-hook-trust` flag can only be auto-injected when the wrapper's **direct program is `codex`** — e.g. `dot-agent-deck wrap --agent codex -- codex --model …`. The wrapper appends the flag to codex's own argv, so it works for the common case (and the deck's default `codex` command).

**If you launch Codex through a launcher or wrapper script** — `devbox run …`, a `run_codex_agent.sh`, an alias, a custom absolute path whose basename is not `codex` — the wrapper sees the *launcher* as the program and **cannot** reach inside it to append the flag to the eventual `codex …` invocation. In that case:

- The deck still **auto-installs the hooks** into `CODEX_HOME` whenever it spawns a Codex-identity pane (i.e. `DOT_AGENT_DECK_PANE_ID` is set), so `hooks.json` is present however codex is ultimately launched — you do **not** need to install hooks in your script.
- You **must add the trust flag yourself** inside the script, on the codex command line:

  ```sh
  #!/bin/sh
  # run_codex_agent.sh — a launcher the deck wraps as
  #   dot-agent-deck wrap --agent codex -- ./run_codex_agent.sh
  exec codex --dangerously-bypass-hook-trust "$@"
  ```

  (Alternatively, pre-trust the deck's hooks once via Codex's interactive `/hooks` review; the persisted trust then applies to subsequent runs without the flag.) Without one of these, Codex will refuse to run the deck's hooks and the dashboard card will fall back to the coarse stdout classifier — reliable status is degraded and no tool/prompt detail appears.

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
- **A skip harness** — add a `check_<agent>_available` helper (and credential import if the agent needs auth) to [`tests/common/mod.rs`](../../tests/common/mod.rs), modelled on `check_codex_available`, so a missing/unauthenticated CLI cleanly *skips* the real-agent test rather than failing it.

**The behaviour-preserving constraint.** For the *existing* agents, the registry move (and any refactor along the way) must be **behaviour-preserving**: the existing test suite must pass **unchanged**. Do not edit an existing test to make it green — if it needs editing, the change altered observable behaviour and that is a bug, not a test update. New coverage for your agent is *additive* on top of the untouched existing suite. Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` per task, and `cargo test-e2e` before the PR.

## Follow-up agents built on this seam

Two follow-up PRDs build directly on the PRD #20 machinery documented here:

- **Gemini** — a **wrapper**-strategy agent, so a thin registry entry + a Gemini-specific `classify_line` rule set + detection + e2e. It reuses `dot-agent-deck wrap` wholesale; the PRD is small *because the wrapper strategy already exists*.
- **Aider** — introduces the **new log-watcher** strategy (`dot-agent-deck watch --agent aider --log <path>` tailing Aider's structured logs into `AgentEvent`s). That PRD carries the one-time log-watcher `IntegrationStrategy` implementation; every log-watching agent after it is back on the cheap path.

[`AgentSpec`]: ../../src/agent_registry.rs
[`AgentType`]: ../../src/event.rs
[`IntegrationStrategy`]: ../../src/agent_registry.rs
[`RuleSet`]: ../../src/wrap.rs
[`LiveTarget`]: ../../src/event.rs
[`TargetKind`]: ../../src/event.rs
[`Writable`]: ../../src/event.rs
[`SendResult`]: ../../src/event.rs
[`AGENT_EVENT_SCHEMA_VERSION`]: ../../src/event.rs
