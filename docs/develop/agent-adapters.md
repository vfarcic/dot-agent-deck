# Agent adapters — adding a new agent

> **Developer / maintainer reference.** This page is the contract for adding a new agent to dot-agent-deck. It is intentionally excluded from the published documentation site and renders as plain Markdown here on GitHub.

dot-agent-deck is a control plane over **external** agent processes: it spawns them, observes their status, and coordinates them. It does not run an agent loop of its own. "Adding an agent" therefore means teaching the deck two things — how to *recognise* the agent, and how the agent's activity *reaches* the deck as [`AgentEvent`](../../src/event.rs)s — and then rendering the result. This guide documents the seams that carry those two things and walks the whole change end to end, using **Codex** (the wrapper-strategy agent shipped by [PRD #20](../../prds/done/20-multi-agent-support.md)) as the worked example.

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

**If you are reusing the `Wrapper` strategy (the cheap path — Codex, Gemini):** you write *no new mechanism*, only *data*. Add a [`RuleSet`] in [`src/wrap.rs`](../../src/wrap.rs) and select it by agent type in `ruleset_for`. **Write its markers against a capture of the process the wrapper actually spawns**, not against a documented output mode it does not launch — Codex's set is the cautionary case (issue #540):

```rust
pub static CODEX: RuleSet = RuleSet {
    // Interactive `codex` paints ANSI redraws and no JSON record, so there is
    // no line that means "error" or "turn over". Empty rather than GENERIC:
    // GENERIC's word markers would flip a working card to Error whenever the
    // redrawn conversation mentions "error".
    error_markers: &[],
    idle_markers: &[],
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

Stdout scraping cannot reach full parity for *interactive* Codex — bare `codex` paints an ANSI TUI on stdout with no JSON, so the classifier only ever sees a wall of redraw text: it reports that the TUI printed something, never `Idle` or `Error` mid-session, and no tool or prompt detail. Until issue #540 the `CODEX` set matched `codex exec --json`'s `"type":"error"` / `"type":"turn.completed"` records, which read as a working classification path while matching nothing the spawned process prints — issue #540's direct PTY captures of an idle boot and of a full turn found no JSON record at all. Codex 0.144.4, however, ships a **Claude-Code-compatible native hooks engine**. So Codex keeps `IntegrationStrategy::Wrapper` as its **PTY host + hook injector**, but its rich events come from **native hooks**, not the classifier.

**A Codex card's status comes from its hooks, not from its output (issue #1493).** `AppState::apply_event` lets a Codex frame marked `wrapper_output_classified` assert no status while the hooks are trusted, so the card is Idle from the wrapper's fork-time start until `UserPromptSubmit`, and Idle again from `Stop` however much the TUI redraws. When the wrapper stamps `wrapper_prompt_reports_unavailable` (below), output is the only sign of a turn, and the wrapper's supervisory loop runs `QuietOutputIdle`: a classified `Idle` once the child has written nothing for `CODEX_QUIET_OUTPUT_IDLE` (3 s, measured against Codex 0.160.0: its idle TUI is silent after the boot paint, while a turn redraws its elapsed-time header at least once a second), and a classified `Thinking` on the next byte. In that mode a classified frame may only move Idle → Thinking or Thinking → Idle, so a hook that does run still owns Needs Input, Working and Error. The wrapper's output-settled `SessionStart` also marks the quiet detector Idle, since the card reads it as Idle. Classified Codex frames do not acknowledge inbox receipt. The other source of a false Working was the shell-activity monitor, which read the wrapper's own child — leader of a session on the wrapper's inner pty — as a detached shell command; see [`shell-activity-signal.md`](shell-activity-signal.md).

**What the classifier's events prove, and what they do not.** Every event the classifier emits is marked `wrapper_output_classified` (issue #714): it proves output, not work, so it cannot clear a Blocked card. And no wrapper event ever carries a submitted prompt — the emitter hardcodes `user_prompt: None` — so inbox receipt uses the native hooks or an explicit inbox fetch. When the trust step in item 2 below does not trust the deck's own `UserPromptSubmit` entry with the user's `/hooks` toggle on — it could not run, recorded nothing, recorded only other hooks, or found the prompt hook switched off — the wrapper stamps `wrapper_prompt_reports_unavailable` on everything it emits, and the wrapper uses the quiet-output status fallback. The marker does not settle inbox messages.

Concretely, when the deck starts (and again whenever the wrapper launches a real `codex`):

1. It installs a `hooks.json` into the active `CODEX_HOME` ([`src/codex_hooks_manage.rs`](../../src/codex_hooks_manage.rs)) whose every command hook shells `dot-agent-deck hook --agent codex`. Those hook payloads are the **same shape Claude posts**, so they are ingested by the existing [`src/hook.rs`](../../src/hook.rs) `handle_hook` `"codex"` arm (stamping `AgentType::Codex`) — no new wire, no `PROTOCOL_VERSION` bump. The installed rule set covers the lifecycle (`SessionStart`/`Stop`), the prompt (`UserPromptSubmit`), tools (`PreToolUse`/`PostToolUse`), permission, compaction, and subagent boundaries — the same class Claude delivers. The installer is **safe for the user's real `~/.codex`**: it resolves `CODEX_HOME` the way Codex does (`$CODEX_HOME`, else `$HOME/.codex` — the user's REAL home in production, never a throwaway), **merges** (a user's own hooks and `config.toml` are never clobbered), identifies its OWN entries by the **exact command signature** `… hook --agent codex` (so a user hook that merely mentions `dot-agent-deck` is preserved), writes **atomically** (temp file + `rename(2)`, serialized by an in-process lock, so a crash or two concurrent Codex spawns can't corrupt the file), and **never discards** existing content it can't parse (malformed JSON is backed up to `hooks.json.bak` and the install errors; a structurally-incompatible shape errors without touching the file).

   **The merge preserves POSITION, not just content, and that is a correctness requirement rather than tidiness (issue #1034).** Codex keys a trust grant positionally — `<sourcePath>:<event_snake>:<group_idx>:<handler_idx>`, the rule's index in the event array and the handler's index inside that rule — and index assignment is **file**-positional rather than listing-positional — measured on issue #1034 by placing a handler Codex skips (`prompt`, unsupported on 0.149.0) at index 0 and seeing the command handler after it still report `:0:1`, so the skipped entry consumed its index. Since `<CODEX_HOME>/hooks.json` is the user's own user-scope file (`source: "user"`), leaving a user's rule byte-identical while *moving* it still breaks the grant attached to it. The install used to strip its own rule out of an event array and push a fresh one onto the end, which moved everything after it; measured against codex-cli 0.149.0, a user's hook at `pre_tool_use:1:0` reporting `trustStatus: trusted` came back at `:0:0` reporting `modified`, and their `[hooks.state]` row was not merely orphaned but **overwritten**, because the deck's rule then landed on `:1:0` and the trust write updates whatever row is already at a key. So `install_impl` now refreshes its own handler's `command` **where it already sits** (`refresh_deck_rule_in_place`), rewriting only that string — `async`, `timeoutSec`, a user's hand-added keys and the rule's `matcher` all survive, which also keeps `currentHash` stable across an unchanged re-install, since that hash is taken over command + `matcher` + `async`. Only *removal* shifts, and appending at the end shifts nothing, so the no-deck-rule-yet fallback is safe. Two removals remain and are documented at the function: a rule emptied by the surplus sweep is **kept** as an empty object (it consumes its `group_idx`), and the retired-event sweep still shifts because removal is precisely its job. This is Codex-specific: it is the only one of the four adapters that writes a trust record at all (`hooks_manage`, `devin_hooks_manage` and `opencode_manage` mention trust nowhere), so the in-place refresh lives in `codex_hooks_manage` rather than in the shared `strip_deck_commands`, whose other three callers would gain risk from it and no benefit.

   The command string is quoted for the shell that will **execute** it rather than for whatever host compiled the deck (issue #734). Codex hands the whole string to a shell — `%COMSPEC%` else `cmd.exe` with `/C` on Windows, `$SHELL` else `/bin/sh` with `-lc` elsewhere (`codex-rs/hooks/src/engine/command_runner.rs`) — so [`agent_hook_config::build_command`](../../src/agent_hook_config.rs) emits `cmd.exe` double quotes on a Windows host and POSIX single quotes otherwise. It only shows up for a Windows user who sets `$CODEX_HOME`, since Codex hook installation is skipped there otherwise, and it is a *different* question from the deliberately always-POSIX word `platform::paths::posix_command_word` builds for the ```` ```bash ````-fenced delegate / work-done instructions.
2. It records **scoped, per-hook, hash-pinned trust** for exactly the entries it just authored (PRD #20 §4.1). Codex requires non-managed command hooks to be *trusted* before they run (an interactive `/hooks` review otherwise), and it keeps that trust in `$CODEX_HOME/config.toml` as `[hooks.state."<sourcePath>:<event_snake>:<group_idx>:<handler_idx>"] { enabled, trusted_hash }` — of which **the deck writes `trusted_hash` and nothing else**. The sibling `enabled` key is a *user* knob, toggled from Codex's own `/hooks` browser ("Turn hooks on or off. Your changes are saved automatically."), where `toggle` is a separate action from `trust`; measured against codex-cli 0.149.0 it is fully orthogonal to trust (an entry with `enabled: false` and a correct hash still reports `trustStatus: trusted` and is still enumerated, and an *absent* `enabled` key reads back as `true`), and distrust is expressed by an absent or stale `trusted_hash` instead. The deck used to write `enabled = true` unconditionally, which silently reverted a deliberate "turn this hook off" on every wrapped Codex spawn; it no longer writes the key at all, which also makes its record identical in shape to Codex's own trust write (issue #730). The deck asks Codex itself for the identity of each hook — `codex app-server` (stdio JSON-RPC) → `hooks/list { cwd }` returns every hook's `key`, `currentHash`, `trustStatus`, `sourcePath`, `command`, and `isManaged`, with no credentials and no network — and then writes a trust record **only** for entries that pass all three parts of the security predicate (`deck_owned_entries`): the `sourcePath` is the pinned home's own `hooks.json`, the command is byte-exactly the one this install wrote for the durable path it resolved, and `isManaged` is false. Where a listing omits `isManaged` altogether — protocol drift; 0.149.0 carries it on every entry across four handler shapes — that third condition is resolved *per direction*: a trust write treats the absent field as managed and refuses the entry (fail closed), while the revocation treats it as unmanaged and still clears the record (fail open in the revoking direction, so `hooks uninstall` cannot leave trust behind). Everything else — a foreign command in the *same* `hooks.json`, a deck-shaped command sourced from a different home, a managed hook, a user command that merely mentions `dot-agent-deck` — stays untrusted. The `config.toml` edit is **format-preserving** (`toml_edit`: only the `hooks.state."<key>"` tables are inserted/replaced), atomic (temp + `rename(2)`), and idempotent, so the user's comments, `model = …`, auth references, and their own trust records survive byte-intact. How long the deck waits for that `hooks/list` reply depends on the path: the wrapper, which is about to launch Codex in a pane, waits up to 30 s (`SPAWN_HOOKS_LIST_TIMEOUT`), while the daemon's startup install and `hooks install` wait 5 s (`HOOKS_LIST_TIMEOUT`). A reply that misses the deadline leaves the hooks untrusted, and Codex 0.160.0 then opens that pane on its hook-review dialog. Five seconds was too short at spawn on a starved box: PRD #1631 measured 3.4–27.7 s for a fresh Codex home there, against 0.2–2 s for homes that had answered before. The wait returns as soon as the reply arrives, so a slow `codex app-server` delays Codex's launch by as long as it takes to answer, up to the whole 30 s when it never does. That 30 s bounds the listing wait, not the pane: the `hooks.json` install and the `config.toml` trust write each take a cross-process lock that waits up to 10 s, so the whole spawn prep can hold Codex's launch for about 50 s. Bounding the prep with one deadline is not done yet. A slow prep delays only the pane: a task for that Codex waits in its inbox, and the doorbell does not ring until the deck recognises Codex's empty composer on screen (PRD #1666), so no fallback timer can expire into a pane Codex is not yet reading. The startup install keeps the short bound, and since issue #1665 it runs its trust write on a thread of its own: only the `hooks.json` write happens before the daemon binds its sockets (and before the TUI draws), so a `codex app-server` that never answers no longer holds either back for those 5 s. A Codex in a deck pane does not depend on that background write, since the wrapper records its own trust before launching it; a Codex started where the wrapper does not run can start before the background write lands, and then meets the hook-review dialog once, as it already did when the startup listing timed out. The listing's `codex app-server` is started without any of the deck's `OTEL_` variables, whichever path runs it: it is the deck asking Codex a question, and with them it exported its own traces, as `codex-app-server`, to the deck's telemetry destination.

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

If Codex still won't run the deck's hooks, the usual cause is a launcher that re-exports `CODEX_HOME` (the deck's pin only reaches the child it spawns) — drop that re-export, or trust the deck's hooks once through Codex's interactive `/hooks` review in that home. Until then the card falls back to the coarse stdout classifier: degraded status, no tool/prompt detail. When it is the wrapper's own trust step that failed, automatic prompts into that pane are also written once and not retried — see *What the classifier's events prove* above. A `CODEX_HOME` re-export is **not** caught that way (issue #559's residual): the wrapper did record trust, in the home it pinned, so it declares nothing, and the deck keeps treating the pane as able to confirm a delivery through hooks that never fire.

#### Inbox wake and wrapper observations

Agent adapters deliver tasks through the inbox, rather than using wrapper observations to time task-body writes. Wrapper session origins remain status observations; they neither acknowledge messages nor authorize a typed task payload. A new adapter must declare native wake only when its agent-side component can arm an authenticated consumer for the managed generation. Otherwise it uses the guarded doorbell and explicit inbox fetch.

The adapter must acknowledge the agent's receipt of a message, rather than a successful API call or PTY write. See [Agent messaging](agent-messaging.md#consumers-and-acknowledgement) for the per-agent receipt signals. Keep the launch command and wrapper decision frozen across status updates: a learned badge must not mutate the command used on restart. Cover direct commands and launchers in the adapter's tests.

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
- **Wrapper `RuleSet` classification tests** (if reusing the wrapper) — that realistic agent output lines map to the right `DetectedEvent`. See `codex_adapter.rs` (`codex_wrap_001_codex_output_is_activity_only`, which pins that interactive Codex output never classifies as `Error` or `Idle`) and the pure-function tests in `src/wrap.rs`.
- **A synthetic e2e** (`e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`) — a PTY-attached test driving a deterministic stand-in that emits realistic agent output, asserting the event stream *and* the visible dashboard card. See `codex_wrap_001_synthetic_codex_reaches_dashboard` in [`tests/e2e_codex_wrapper.rs`](../../tests/e2e_codex_wrapper.rs).
- **A real-agent e2e** — the same PTY-attached shape, but driving the *real* agent on a **cheap model** through a cheap, deterministic-enough operation (list a directory and report a uniquely-named fixture **sentinel file**, so the assertion survives LLM phrasing variance). See `codex_live_001_real_model_lists_sentinel_in_wrapped_pane`. Real-agent tests live in **lane 2** — the `e2e-live` half of the L2 suite, which since issue #502 runs on a developer's machine and **nowhere in CI**, because no test that reaches a real agent runs on a runner ([`e2e-lanes.md`](e2e-lanes.md)). So a new adapter's real-agent test is only as good as the last time somebody ran `cargo test-e2e-live <filter>`: run it yourself before trusting it, and again whenever you touch the adapter. [CLAUDE.md rule 4](../../CLAUDE.md) is the bar: **at least one test per major feature must validate it as a user actually uses and sees it.**
- **A skip harness** — add a `check_<agent>_available` helper (and credential import if the agent needs auth) to [`tests/common/mod.rs`](../../tests/common/mod.rs), modelled on `check_codex_available`, so a missing/unauthenticated CLI cleanly *skips* the real-agent test rather than failing it. Keep the model the gate probes and the model the tests launch the SAME value (for Codex, `common::codex_test_model()`), or the suite can skip for a reason the scenario would not have hit — a skip reads as a pass, so a wrong gate is silent no-coverage. The two Codex auth modes reach different model families, so since issue #656 that helper picks its default from `auth_mode` in `~/.codex/auth.json` — `gpt-5.6-luna` for a ChatGPT login, `gpt-5-nano` for an API key — and a logged-in Codex that refuses the model fails the test instead of skipping it. `DOT_AGENT_DECK_CODEX_TEST_MODEL` still overrides both. [`e2e-lanes.md`](e2e-lanes.md) has the measurements.

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

### OpenCode 1.x and 2.x — one plugin for both (PRD #2050)

OpenCode 2 does not run 1.x plugins, so the `Plugin` strategy carries both generations. The user-facing half is [`docs/installation.md`](../installation.md#opencode-1x-and-2x); what a maintainer needs on top of it:

- **One server module, dual default export** (`{ id, server, setup }`): 1.18.29+ calls `server`, 2.x calls `setup`. 1.19.5 also calls `setup` (with no `event` domain), so `setup` stays inert once `server` has run in the process. The floor is `agent_notice::OPENCODE_SUPPORTED_FLOOR`.
- **The installed files do not depend on the version** ([`src/opencode_manage.rs`](../../src/opencode_manage.rs), `Layout`): wherever a global config root exists, the module goes in the existing global roots (`$XDG_CONFIG_HOME/opencode`, which 1.x loads, and `$OPENCODE_CONFIG_DIR` when set, which 2.x then loads instead of the XDG root), removes the deck's copy from `~/.opencode` once 2.x's global root holds the module (2.x also scans `~/.opencode` for a project under `$HOME` and refuses a duplicate plugin id), and installs both TUI halves, which ignore each other. A detected 2.x creates its global root when none exists; otherwise a machine with only `~/.opencode` gets the module there, and the next install, after the TUI half created the XDG root, moves it once. The module carries no layout marker: an earlier build of this branch wrote one on its first line, which made every change of generation rewrite the module. It is still read as a fallback.
- **OpenCode 2 reloads its plugins when the module file changes** (measured on 2.2.5 in lane 2: a `SessionEnd`, then a new `SessionStart`, and the turn's idle never reported). So a write of the module reaches each live 2.x pane that loads it, mid-turn, and the installers must leave unchanged bytes alone: a write happens only for a template change (a deck upgrade costs one reload) or a pin change. The module declares `DAD_TEMPLATE_REVISION` (`PLUGIN_TEMPLATE_REVISION`, which `the_template_revision_is_bumped_with_the_template` keeps moving with the template), and an automatic install leaves a module of a later revision alone while its pin is usable, so an older daemon still running after an upgrade does not put its template back. A daemon built before that check does write its own template back. `hooks install --agent opencode` writes this binary's template regardless. Whether 2.x also reloads on a `tui.json`, `cli.json` or `plugins/*/tui.js` change has not been measured; those files too are written only when their bytes change.
- **The version is still detected** ([`src/opencode_version.rs`](../../src/opencode_version.rs)), for `--standalone`, the card notices, and the record the command-less installs read. A direct command's program is resolved on the child's `PATH` and asked `--version`, cached by the binary's file identity so an in-place upgrade is asked again; that version also travels with the pane (`RunningAgent.opencode_spawn_version`, then `Watched.spawn_version`) as the version of the pane's client, which covers an npm `#!/bin/sh` launcher that `exec`s another binary. For a wrapper, the file `opencode` resolves to on the pane's `PATH` (else the daemon's login-shell `PATH`) is probed the same way (`probe_path_opencode`), and the watch matches a client's executable against those probes by file identity, running nothing it finds. The generation last seen goes to `<state dir>/opencode-layout` (`remember_layout`), never into OpenCode's config; the command-less installs (startup, `hooks install`, `hooks refresh`) and a wrapper's spawn read it first (`remembered_major_now`), then an older build's module marker. All it changes is whether 2.x's global root is created on a machine that has none.
- **A binary replaced in place** (`opencode upgrade`, an auto-update, a reinstall) reads `…/opencode (deleted)` through `/proc/<pid>/exe` on Linux; the OpenCode name checks on an executable link strip that suffix first (`opencode_mode::exe_is_opencode`, and `v2Parent` in the module), as `is_shell` in `src/daemon.rs` does.
- **2.x runs server plugins in a shared background service** unless the client is `--standalone` (or the service is disabled). `with_standalone` adds the flag to a direct `opencode` command at spawn only; the recorded `spawn_command` keeps the user's text.
- **The card notices come from the pane's process tree** ([`src/opencode_mode.rs`](../../src/opencode_mode.rs), polled by the daemon): a 2.x client without an `opencode serve --stdio` child is on the shared service; an OpenCode with no hook event and no armed receiver after a grace is "not working". Only panes whose spawn declared or revealed OpenCode are watched, which is why an undeclared wrapper gets no notice. The sentences live in [`src/agent_notice.rs`](../../src/agent_notice.rs) and travel as the additive optional `InboxCardNotices.agent_notice`, so both clients print the daemon's words.
- **Inbox receiver admission on 2.x** resolves the private `serve --stdio` server to its parent client (`opencode_server_client` in `src/daemon.rs`), so the server plugin and the TUI half pin one process, as 1.x's single process did.
- **Forms**: 2.x's server-plugin context has no form reply, so only the TUI half answers forms; `opencode mini` loads no TUI plugin and reports a form as answer-in-OpenCode.

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
