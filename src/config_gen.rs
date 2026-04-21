/// Prompt template for instructing an AI agent to generate a `.dot-agent-deck.toml`
/// configuration file by analyzing the project structure.
const CONFIG_GEN_PROMPT_TEMPLATE: &str = r#"Analyze this project and create a `.dot-agent-deck.toml` configuration file in the project root.

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

**`init_command`** (optional): A setup command that runs once in every pane before its own command starts. Use it when the project requires an environment setup step — e.g., `devbox shell`, `nvm use`, `source .env`, `conda activate env`. Only add it if the project actually needs it (look for devbox.json, .nvmrc, .env, conda environment files, etc.). Most projects don't need it.

**Persistent panes** run automatically on a 10-second refresh cycle (`watch = true` by default). Write the plain command without any watch/follow/polling flags — the system handles refresh internally. Examples:
- `command = "kubectl get pods -o wide"` — refreshes every 10s automatically
- `command = "helm list -A"` — refreshes every 10s automatically

Set `watch = false` ONLY for commands that have their own built-in streaming (e.g., `kubectl get pods -w`, `tail -f /var/log/app.log`, `cargo watch -x test`). These run directly without the system refresh wrapper.

Do NOT use the `watch` binary, `while true` loops, or any other external polling mechanism — the system handles this.

**Reactive rules** capture commands the agent runs. **The matched command itself is what gets re-executed in the side pane.** This is critical to understand:
- `watch = false`: the matched command runs once in the side pane and the output stays visible
- `watch = true`: the matched command is re-executed repeatedly on the interval timer
- **ONLY create rules for read-only commands** (get, list, status, top, describe, logs, diff, show, cat, tree). NEVER create rules matching commands that mutate state (apply, install, deploy, create, delete, upgrade, update, patch, scale, rollout, helm install, helm upgrade, terraform apply). The system re-executes matched commands, so write operations would be duplicated
- Regex patterns use Rust regex syntax (escape backslashes: `\\s`, `\\d`, etc.)

**CRITICAL: Every command in the config MUST come from the project's own toolchain.** If the project uses Helm and kubectl, propose Helm and kubectl commands. If it uses Go, propose Go commands. NEVER propose commands from ecosystems the project does not use — e.g., do not propose `cargo` commands for a Python project, or `npm` commands for a Kubernetes infra project.

**Cover the project's tooling.** Create rules for all the read-only commands the agent is likely to use during development. If you have more reactive rules than the default 2 reactive pane slots, increase `reactive_panes` accordingly (e.g., `reactive_panes = 3` for 3 rules). Keep persistent panes to 1-2 to avoid crowding the screen.

**Compact output is essential.** Side panes are small — roughly 80 columns wide and 15-20 rows tall. Commands must produce concise output that fits this space. Avoid `-o wide`, verbose flags, or commands that produce wide tables. Prefer narrow output formats: use `-o name`, column selection, `--no-headers`, or pipe through `awk`/`cut` to trim columns. If a command naturally produces many lines, add `| head -20` or similar limits.

**Watch intervals**: when using `watch = true`, prefer longer intervals (10-30 seconds) unless the user needs near-real-time updates. Fast refresh (2-5 seconds) creates visual noise and unnecessary load.

## Your Task

1. **Examine the project at `{dir}` first.** Read build files, config files, and scripts to identify the actual languages, frameworks, and tools this project uses. Do NOT assume any specific toolchain — discover it from the project files:
   - Build/package files (e.g., package.json, go.mod, pyproject.toml, Makefile, Cargo.toml, pom.xml, etc.)
   - CI/CD configs (.github/workflows/, Jenkinsfile, etc.)
   - Scripts directory and any task runners
   - Infrastructure config (Dockerfile, docker-compose, Helm charts, Terraform files, etc.)

2. **Validate every command exists.** For every binary used in pane commands or that could match rule patterns, run `which <binary>` to confirm it is installed. Do NOT include any command that uses a binary not found on the system. If a useful tool is not installed, mention it as a suggestion but exclude it from the config.

3. **Propose** a `.dot-agent-deck.toml` with a **single mode** tailored to this project:
   - Pick the most useful workflow for AI-assisted development
   - Choose persistent panes for commands developers run continuously
   - Choose reactive rules for commands you'll likely execute during AI-assisted work
   - Use meaningful mode and pane names
   - Aim for about 3 side panes total (persistent + reactive combined)

4. **Present your proposed config to the user before writing it.** For each pane and rule, briefly explain why you chose it. Ask the user to confirm, modify, or remove items. Only write the file after the user approves.

5. Write the approved config to `{dir}/.dot-agent-deck.toml`

6. **After writing the file, tell the user the next steps:** "Config created! To use it, press Ctrl+w to close this pane, then Ctrl+n to create a new one. Select the same directory and choose your mode from the Mode field."

## Orchestrations (Optional)

After proposing modes, ask the user if they want to set up **agent orchestrations** — multi-agent workflows where a designated orchestrator agent delegates tasks to worker agents. Examples: code + review, TDD cycles, code + security audit.

If the user wants orchestrations, guide them through defining one:

### Orchestration Config Format

```toml
[[orchestrations]]
name = "orchestration-name"   # e.g., "code-review", "tdd-cycle"

[[orchestrations.roles]]
name = "orchestrator"         # The orchestrator role — delegates work, never does it
command = "claude"            # CLI command to launch the agent
start = true                  # Exactly one role MUST have start = true
prompt_template = """         # Optional: base instructions for the orchestrator
You are an orchestration manager. You NEVER do work yourself.
You only delegate to available agents and coordinate their work.
"""

[[orchestrations.roles]]
name = "coder"                # Worker role name (must be unique across all roles)
command = "claude --model sonnet"
description = "Implements code changes, fixes bugs, writes features"  # Used to build the orchestrator's agent list
prompt_template = "Always run cargo test before finishing."  # Optional: standing instructions prepended to each task
# clear = true                # Default true — restart agent session between delegations for context isolation

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code for correctness, style, and edge cases"
```

### Orchestration Guidelines

- **Exactly one `start = true` role**: This is the orchestrator. It receives the user's initial request and delegates to workers. dot-agent-deck auto-appends the available agents list and delegation protocol to the orchestrator's prompt.
- **At least 2 roles** per orchestration (one orchestrator + at least one worker).
- **All role names must be unique** within the orchestration.
- **Role names must not be empty** and must not contain `..`, `/`, `\`, or null characters (they are used in file paths).
- **Every role must have a non-empty `command`**.
- **Worker `description`** is important — it's used to auto-build the orchestrator's available agents list. Be specific about what each worker does.
- **Worker `prompt_template`** is optional standing instructions (e.g., "always run tests before finishing"), NOT task instructions. The orchestrator provides task instructions each time via delegation.
- **`clear = true` (default)**: Restarts the agent session between delegations for context isolation. Set `clear = false` if the agent needs to retain state across delegations.
- **`command`**: The CLI command to launch the agent. Use the agent CLIs available on the system (e.g., `claude`, `opencode`). Run `which <binary>` to verify availability.
- **Suggest orchestrations that match the project's workflow**: code + review for teams that care about code quality, code + security audit for security-sensitive projects, etc.

### Example Orchestrations

**Code Review** (code + review):
```toml
[[orchestrations]]
name = "code-review"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements code changes, fixes bugs, writes features"

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code for correctness, style, and edge cases"
```

**Code + Security Audit**:
```toml
[[orchestrations]]
name = "secure-dev"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements code changes and features"

[[orchestrations.roles]]
name = "security-auditor"
command = "claude"
description = "Audits code for security vulnerabilities and OWASP top 10 issues"
```

### Orchestration Task

1. After the modes section is confirmed, ask: "Would you also like to set up an agent orchestration? This lets multiple AI agents collaborate — e.g., one writes code while another reviews it."
2. If yes, ask about:
   - Orchestration name (a short workflow name like "code-review")
   - Worker roles: name, command, description for each
   - Whether any workers need custom `prompt_template` (standing instructions)
   - Whether `clear = false` is needed for any worker (uncommon)
3. Always include an orchestrator role with `start = true`
4. Validate: exactly one `start = true`, at least 2 roles, all role names unique and non-empty (no `..`, `/`, `\`), all commands non-empty
5. Present the proposed orchestration config and get user approval
6. Write the `[[orchestrations]]` section into `.dot-agent-deck.toml` alongside the `[[modes]]` section

## Quality Guidelines

- **Discover, don't assume.** Only propose commands for tools the project actually uses. Never check for or include tools from unrelated ecosystems.
- **Focused output over broad output.** Persistent panes should show actionable, scoped information — not everything.
- **Only use installed tools.** Every command in the config must work on this system right now.
- **Fewer is better.** The user can always add more panes later.
- **Orchestrations are optional.** Only suggest them if the user is interested. Don't force multi-agent workflows on simple projects."#;

/// Build the config generation prompt for a specific directory.
pub fn config_gen_prompt(dir: &str) -> String {
    CONFIG_GEN_PROMPT_TEMPLATE.replace("{dir}", dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_interpolates_directory() {
        let prompt = config_gen_prompt("/home/user/my-project");
        assert!(prompt.contains("/home/user/my-project"));
        assert!(!prompt.contains("{dir}"));
    }

    #[test]
    fn prompt_contains_key_sections() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("[[modes]]"));
        assert!(prompt.contains("[[modes.panes]]"));
        assert!(prompt.contains("[[modes.rules]]"));
        assert!(prompt.contains(".dot-agent-deck.toml"));
    }

    #[test]
    fn prompt_contains_orchestration_sections() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("[[orchestrations]]"));
        assert!(prompt.contains("[[orchestrations.roles]]"));
        assert!(prompt.contains("start = true"));
        assert!(prompt.contains("Orchestrations (Optional)"));
    }

    #[test]
    fn prompt_contains_orchestration_guidelines() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("Exactly one `start = true` role"));
        assert!(prompt.contains("All role names must be unique"));
        assert!(prompt.contains("prompt_template"));
        assert!(prompt.contains("description"));
        assert!(prompt.contains("clear"));
    }

    #[test]
    fn prompt_contains_orchestration_examples() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("code-review"));
        assert!(prompt.contains("secure-dev"));
        assert!(prompt.contains("security-auditor"));
    }
}
