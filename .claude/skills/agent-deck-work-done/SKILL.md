---
name: work-done
description: "Signal that you have completed your current task. Notifies the orchestration daemon with a structured summary of your work."
user-invocable: true
---

# Work Done

You have completed your current task. Summarize your work and signal completion via the CLI.

## Instructions

Compose a concise summary of what you accomplished, then run the appropriate command below.

### Worker agent (completed your assigned task)

```bash
dot-agent-deck work-done --task "Your summary here. Include file paths, outcomes, and anything the next agent needs to know."
```

### Orchestrator (delegating work to one agent)

```bash
dot-agent-deck work-done --delegate <role-name> --task "Task description with relevant context, file paths, and constraints."
```

### Orchestrator (delegating to multiple agents in parallel)

Make **one call per agent** so each gets its own task description:

```bash
dot-agent-deck work-done --delegate coder --task "Implement the login endpoint..."
dot-agent-deck work-done --delegate reviewer --task "Review the auth module..."
```

If all agents should receive the **exact same task**, you may combine them:

```bash
dot-agent-deck work-done --delegate <role1> --delegate <role2> --task "Same task for all."
```

### Orchestrator (all work is complete)

```bash
dot-agent-deck work-done --done --task "Final summary of what was accomplished across all agents."
```

## Rules

- Always include specific file paths, issue numbers, and other references in your `--task` summary.
- Do NOT include full file contents in the summary. The next agent can read files directly.
- The `--task` value should be a single string. Use quotes to wrap multi-sentence summaries.
- The `--delegate` flag accepts role names that match the orchestration config (e.g., `coder`, `reviewer`, `auditor`).
