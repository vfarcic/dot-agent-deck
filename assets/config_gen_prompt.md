Analyze this project and create a `.dot-agent-deck.toml` configuration file in the project root.

## What is .dot-agent-deck.toml?

This file configures "workspace modes" for the dot-agent-deck TUI dashboard. Each mode defines a tab-based workspace that pairs you (the AI agent) with live command output in side panes. When a user activates a mode, a new tab opens with the agent on the left and side panes stacked on the right.

## Config Format

The file is TOML. You can define one or more modes. Each mode has a name, optional persistent panes, and optional reactive rules.

```toml
[[modes]]
name = "mode-name"   # Display name shown in the tab bar
init_command = "devbox shell"  # Optional: runs once in every pane before its command
reactive_panes = 2             # Optional: number of reactive pane slots (default: 2)

# Persistent panes: run when the mode activates, stay alive for the tab lifetime.
# By default, commands are re-executed every 10 seconds automatically (watch = true).
# Set watch = false for commands that stream/follow on their own (e.g., --watch, -f, tail -f).
[[modes.panes]]
command = "some-command"           # Shell command to run (plain, no watch/follow flags needed)
name = "Display Name"              # Optional label (defaults to command)
# watch = true                     # Default: system re-runs command every 10s automatically
                                   # Set to false for commands with built-in streaming

# Reactive rules: regex patterns matched against your bash commands.
# When you execute a command that matches, THE EXACT SAME COMMAND is re-executed
# in a side pane. With watch = true, it is re-executed repeatedly on an interval.
# This means the matched command itself must be safe to re-run!
[[modes.rules]]
pattern = "regex\\s+pattern"   # Regex matched against your bash commands
watch = false                   # false (default): run the matched command once
                                # true: re-run the matched command on interval
interval = 5                   # Seconds between re-runs (only when watch = true)
```

## Guidelines

**`init_command`** (optional): A setup command that runs once in every pane before its own command starts. If the project ships a reproducible-environment manifest you discovered (e.g. `devbox.json`, `flake.nix`, `.nvmrc`, `pyproject.toml` poetry env, `environment.yml` conda, `.tool-versions` asdf, `.envrc` direnv, a project `Makefile` `shell` target), default `init_command` to that activation command — projects that ship one expect commands to run inside it. Skip `init_command` only when the project has no such manifest.

**Persistent panes** run automatically on a 10-second refresh cycle (`watch = true` by default). Write the plain command without any watch/follow/polling flags — the system handles refresh internally. Examples:
- `command = "kubectl get pods -o wide"` — refreshes every 10s automatically
- `command = "helm list -A"` — refreshes every 10s automatically

Set `watch = false` ONLY for commands that have their own built-in streaming (e.g., `kubectl get pods -w`, `tail -f /var/log/app.log`, `cargo watch -x test`). These run directly without the system refresh wrapper.

Do NOT use the `watch` binary, `while true` loops, or any other external polling mechanism — the system handles this.

**Reactive rules** capture commands the agent runs. **The matched command itself is what gets re-executed in the side pane.** This is critical to understand:
- `watch = false`: the matched command runs once in the side pane and the output stays visible.
- `watch = true`: the matched command is re-executed repeatedly on the interval timer.
- Regex patterns use Rust regex syntax (escape backslashes: `\\s`, `\\d`, etc.).
- **Prefer consolidated alternations** like `cargo (test|clippy|check)` or `git (log|status|show|diff)` over many narrow rules — this keeps `reactive_panes` count low and panes from fragmenting.

**Rule safety: NEVER match a command that mutates files, state, or remote systems.** The matched command itself is what gets re-executed (sometimes on a timer), so any side effect would be duplicated. Concrete exclusions:
- Formatters/codegen run without `--check`/`--dry-run`: `cargo fmt`, `gofmt -w`, `prettier --write`, `black`, `rustfmt`, code generators that write files.
- Package operations: `npm install`, `pip install`, `cargo add`, `go get`, `bundle install`.
- Anything that creates, deletes, or modifies state: `apply`, `deploy`, `merge`, `push`, `commit`, `helm install`, `helm upgrade`, `terraform apply`, `kubectl apply`, `kubectl delete`, `make install`, `cargo publish`.

Whitelist (safe to mirror): read-only inspection — `status`, `log`, `diff`, `show`, `list`, `get`, `describe`, `top`, `tree`, `cat`, `head`, `tail` (without `-f`), `tests`, `check`, `clippy`, `lint`, `--check`/`--dry-run` variants of formatters and applies.

**CRITICAL: Every command in the config MUST come from the project's own toolchain.** If the project uses Helm and kubectl, propose Helm and kubectl commands. If it uses Go, propose Go commands. NEVER propose commands from ecosystems the project does not use — e.g., do not propose `cargo` commands for a Python project, or `npm` commands for a Kubernetes infra project.

**Cover the project's tooling.** Create rules for all the read-only commands the agent is likely to use during development. If you have more reactive rules than the default 2 reactive pane slots, increase `reactive_panes` accordingly (e.g., `reactive_panes = 3` for 3 rules). Keep persistent panes to 1-2 to avoid crowding the screen.

**Compact output is essential.** Side panes are small — roughly 80 columns wide and 15-20 rows tall. Commands must produce concise output that fits this space. Avoid `-o wide`, verbose flags, or commands that produce wide tables. Prefer narrow output formats: use `-o name`, column selection, `--no-headers`, or pipe through `awk`/`cut` to trim columns. If a command naturally produces many lines, add `| head -20` or similar limits.

**Watch intervals**: when using `watch = true`, prefer longer intervals (10-30 seconds) unless the user needs near-real-time updates. Fast refresh (2-5 seconds) creates visual noise and unnecessary load.

## Your Task

1. **Discover the project at `{dir}`.** Read its build/package files, task runners, scripts, and CI/CD configs to identify the real toolchain. Do NOT assume — derive everything from what's actually in the repo. Probe for:
   - **Build/package manifests**: `package.json`, `go.mod`, `pyproject.toml`, `Cargo.toml`, `pom.xml`, `Gemfile`, `build.gradle`, `Makefile`, etc.
   - **Task runners**: `Taskfile.yml`, `justfile`, `Makefile`, `package.json#scripts`, `pyproject.toml#tool.poe`, etc.
   - **Reproducible-environment manifests**: `devbox.json`, `flake.nix`, `shell.nix`, `.nvmrc`, `pyproject.toml` (poetry), `environment.yml` (conda), `.tool-versions` (asdf), `.envrc` (direnv), etc. — these drive `init_command`.
   - **Project-defined agent launchers**: any script, alias, or documented instruction in the repo that says how to launch an agent CLI (`claude`, `opencode`, `cursor-agent`, etc.). Look in script directories (`scripts/`), task runners (`package.json#scripts`, `Taskfile.yml`, `justfile`, `Makefile`), reproducible-env manifests' script sections, agent-specific configs (`.claude/`, `opencode.json`, `AGENTS.md`, `CLAUDE.md`), and `README`/`CONTRIBUTING`. **Record the full invocation form**, not the bare script name — these are not binaries on PATH, so the role's `command` must include the runner: `devbox run <script>`, `npm run <script>`, `task <recipe>`, `make <target>`, `just <recipe>`, etc. When present, every orchestration role's `command` should use one of these invocations.
   - **Project-defined slash commands the orchestrator can invoke**: agent CLIs like `claude` and `opencode` support custom slash commands and skills, typically kept in CLI-specific paths inside the repo (e.g. `.claude/skills/`, `.claude/commands/`, or the project's opencode config/skills location) or in the user's home (e.g. `~/.claude/skills/`). Discover the relevant paths for whichever CLI the orchestrator will invoke. Anything that looks like *coordination* — progress tracking, status reporting, release/PR automation, navigation between tasks — is a candidate the orchestrator can run *itself* between worker delegations rather than delegating. Reference relevant ones in the orchestrator's `prompt_template`. Skip if the orchestrator's CLI has no such concept or no relevant commands exist.
   - **Spec directories**: any directory the project uses to drive work (e.g. `specs/`, `prds/`, `rfcs/`, `proposals/`, `docs/adr/`). Reference these in worker `prompt_template`s — they're a common context source for delegations.
   - **CI/CD configs**: `.github/workflows/`, `.gitlab-ci.yml`, `Jenkinsfile`, `.circleci/`, etc. — show what gets validated on PRs.
   - **Infrastructure**: `Dockerfile`, `docker-compose.yml`, Helm charts, Terraform, etc.

2. **Validate every command exists.** For every binary that appears in pane commands, rule patterns, or orchestration role `command`s, run `which <binary>` to confirm it is installed. Exclude any command whose binary is not on the PATH. If a useful tool is missing, mention it as a suggestion but do not include it in the config.

3. **Propose** a `.dot-agent-deck.toml` with a **single mode** tailored to this project:
   - Pick the most useful workflow for AI-assisted development.
   - Choose persistent panes for commands developers run continuously (a `git diff --stat HEAD` or `git status -s` is often the right default for AI-paired work — surfaces in-flight changes).
   - Choose reactive rules for read-only commands you'll likely execute during AI-assisted work.
   - Use meaningful mode and pane names.
   - Aim for about 3 side panes total (persistent + reactive combined).

4. **Always propose an orchestration alongside the modes**, composed from the **Role Library** at the bottom of this prompt. Process:
   - **Pick worker roles** from the library that fit this project's workflow. Common picks: `coder` + `reviewer` + `auditor`. Add `tester` if the project uses TDD signals, `documenter` if the project has substantial docs, `researcher` for context-heavy codebases. Propose `release` by default whenever the project has release-flow signals: a release CI workflow, a `/prd-done`/`/release`-style slash command, a `release` task in the task runner, or any push/merge/tag/publish automation. The user can drop it at the proposal step if they'd rather have the orchestrator handle release directly.
   - **Fill in each role's `command`** with the full invocation form of a project launcher (from step 1), matched by *semantic intent*, not just name. Example: a project that defines devbox scripts `agent-big` (opus), `agent-small` (haiku), and `oc-coder`/`oc-reviewer`/`oc-auditor` would yield `command = "devbox run agent-big"` for the orchestrator (benefits from a stronger model), `command = "devbox run agent-small"` for the `release` worker (lighter, mostly runs commands), `command = "devbox run oc-coder"` for `coder`, and so on — the runner (`devbox run`, `npm run`, `task`, etc.) is part of `command`, never stripped. **Do not mix**: if any project launcher is used for one role, every role should use one — extend the project's convention if a perfect match doesn't exist (reuse the closest fit) rather than falling back to a bare CLI for some roles. If the project has no launcher convention at all, fall back to a bare CLI that's on PATH (verify with `which claude`, `which opencode`, etc.). If multiple agent CLIs are present on PATH and there's no project signal pointing to one, ask the user during the proposal step rather than guessing.
   - **Tune each role's `prompt_template`**: start from the library's suggestion, then fill in project-specific details — the actual test command, the spec directory path, the release command name. Keep tuning minimal; the library text is already good for most cases.
   - **Compose the orchestrator's `prompt_template`** from the selected workers. Cover three things:
     - **Workflow shape**: who runs first, what runs in parallel (e.g. reviewer + auditor in parallel after coder), where the user-validation gate sits (typically before `release`). Reference any spec directory you discovered.
     - **Role boundary**: the orchestrator NEVER does *implementation, review, or audit* work — that's worker territory. It MAY run lightweight *coordination* slash commands the project provides (progress tracking, task navigation, status reporting) directly between delegations, without delegating, when those exist. Actions with side effects on shared state — push, merge, tag, publish — are better handled by a dedicated worker (typically `release`): `clear = false` lets it resume after a CI flake without restarting the whole flow, and a restricted-scope prompt prevents the agent from sliding into source edits when something goes wrong.
     - **Context-handoff rule** (see mandatory callout below).
   - Always include an orchestrator role with `start = true`. Validate: exactly one `start = true`, at least 2 roles, all role names unique and non-empty (no `..`, `/`, `\`), all commands non-empty.

   **Context-handoff rule (MANDATORY in the orchestrator's `prompt_template`).** Workers cold-start with `clear = true` by default — they have NO memory of prior conversation, no access to other workers' outputs, and no shared scratchpad. Whatever the orchestrator writes in `--task` is the entire context the worker has, plus the worker's own `prompt_template`. The composed orchestrator `prompt_template` MUST therefore include rules along these lines (adapt the wording to the project, but cover all three points):

   > *"Every delegation must include all context the worker needs: file paths to read, the relevant spec path if applicable, exact error messages when retrying after a failure, and a brief summary of any prior worker's findings when chaining workers (e.g. coder → reviewer)."*
   >
   > *"Do NOT assume workers can see prior conversation or other workers' outputs — paste references explicitly."*
   >
   > *"If the context is long, write it to `.dot-agent-deck/<task-slug>.md` and reference that path in the `--task` description rather than pasting it inline."*

   Lead the proposal with project-specific signals you discovered ("the repo defines an `<alias>` script, so I wired it into the `coder` role") rather than generic boilerplate. If genuinely no orchestration fits the project, say so explicitly — but the default is to propose one.

5. **Present the full proposed config (modes + orchestration) to the user before writing it, with every pane, rule, and role numbered.** Numbering lets the user reference items concisely ("drop rule 2", "rename pane 1", "drop the auditor role"). Briefly explain why you chose each item. Make clear the orchestration is optional and can be dropped entirely while keeping the modes. Close with negative confirmation — e.g., "Tell me what to drop or change, otherwise I'll write the whole thing." Do NOT ask multiple-choice questions like "(a) modes-only or (b) include orchestration?" — those force an extra round-trip when the user could just say what to remove. Only write the file after the user confirms.

6. Write the approved config to `{dir}/.dot-agent-deck.toml`. If an orchestration was kept, include `[[orchestrations]]` alongside `[[modes]]` in the same file.

7. **After writing the file, tell the user the next steps:** "Config created! To use it, press Ctrl+w to close this pane, then Ctrl+n to create a new one. Select the same directory and choose your mode from the Mode field."

## Orchestrations

Reference material for step 4. **Agent orchestrations** are multi-agent workflows where a designated orchestrator agent delegates tasks to worker agents.

### Orchestration Config Format

```toml
[[orchestrations]]
name = "orchestration-name"   # e.g., "code-review", "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"         # The orchestrator role — delegates work, never does it
command = "claude"            # CLI command to launch the agent
start = true                  # Exactly one role MUST have start = true
prompt_template = """         # Composed from selected workers (see step 4)
You coordinate the team. You NEVER do work yourself — only delegate.
[workflow shape: who runs first, what runs in parallel, gates]
"""

[[orchestrations.roles]]
name = "coder"                # Picked from the Role Library
command = "claude --model sonnet"   # Or a project-defined alias if one exists
description = "Implements features, fixes bugs, refactors code"   # From library
prompt_template = "Implement the requested change. Run cargo test before finishing."
# clear = true                # Default true — restart agent session between delegations

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code changes for correctness, style, and edge cases"
```

### Orchestration Guidelines

- **Exactly one `start = true` role**: This is the orchestrator. It receives the user's initial request and delegates to workers. dot-agent-deck auto-appends the available agents list and delegation protocol to the orchestrator's prompt.
- **At least 2 roles** per orchestration (one orchestrator + at least one worker).
- **All role names must be unique** within the orchestration.
- **Role names must not be empty** and must not contain `..`, `/`, `\`, or null characters (they are used in file paths).
- **Every role must have a non-empty `command`**.
- **Worker `description`** is important — it's used to auto-build the orchestrator's available agents list. Use the library's `description` as-is unless the project changes the role's scope.
- **Worker `prompt_template`** is optional standing instructions, NOT task instructions. The orchestrator provides task instructions each time via delegation.
- **`clear = true` (default)**: Restarts the agent session between delegations for context isolation. Set `clear = false` for agents that need to resume after a failure (e.g. a `release` agent retrying after a CI hiccup).
- **`command`**: Use the full invocation form of a project launcher (discovered in step 1), matched by semantic intent — e.g. `command = "devbox run agent-big"`, never just `"agent-big"`, because devbox/npm/task scripts are not binaries on PATH. If the project has no launcher convention, fall back to a bare CLI that is on PATH; if multiple agent CLIs are available with no project signal, ask the user. Verify availability with `which <runner-or-binary>` before including it.
- **Propose exactly one orchestration.** Combine all relevant workers into that single orchestration — the orchestrator routes each task to the right worker. Do NOT present multiple orchestrations as either/or alternatives.

### Worked Example

Suppose you picked `coder` + `reviewer` + `auditor` from the role library and the project has no agent CLI aliases. The result might look like:

```toml
[[orchestrations]]
name = "dev-flow"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
prompt_template = """
You coordinate the team. You NEVER do work yourself — only delegate to the available agents.

Workflow:
- Delegate implementation to coder.
- After coder finishes, delegate to reviewer and auditor in parallel.
- Resolve any blocking findings before moving on.

Context handoff (CRITICAL): every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is the entire context they have. Therefore:
- Always include relevant file paths the worker should read (the spec, the files being modified, etc.).
- When chaining workers (coder → reviewer), summarize the prior worker's relevant findings or list the files they changed.
- When retrying after a failure, paste the exact error message into --task.
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path in --task instead of pasting inline.
"""

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements features, fixes bugs, refactors code"
prompt_template = "Implement the requested change. Run the project's test command before reporting completion."

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = "Review the change. Report findings only — do not modify code."

[[orchestrations.roles]]
name = "auditor"
command = "claude"
description = "Audits code for security vulnerabilities and unsafe patterns"
prompt_template = "Audit the change for security vulnerabilities. Report findings only."
```

When proposing a `release` role, set `clear = false` on it so it can resume after a CI failure, and add a user-validation gate to the orchestrator's `prompt_template`: "Before delegating to release, summarize what to test end-to-end and STOP until the user confirms."

## Role Library

The following generic roles are pre-defined. Pick from these when composing an orchestration. For each entry: `description` is what the role does (use as the role's `description`), `clear` is the recommended default, and `prompt_template` is a starter you should tune to the project (substitute the actual test command, spec directory, release command name, etc.).

{roles}

## Quality Guidelines

- **Discover, don't assume.** Only propose commands for tools the project actually uses.
- **Focused output over broad output.** Persistent panes should show actionable, scoped information.
- **Only use installed tools.** Every command in the config must work on this system right now.
- **Fewer is better.** The user can always add more panes or roles later.
- **Always propose an orchestration.** Drop only if no role from the library plausibly applies.
