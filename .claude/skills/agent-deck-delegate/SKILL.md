---
name: delegate
description: "Delegate work to one or more worker agents. Only the orchestrator should use this command."
user-invocable: true
---

# Delegate

You are the orchestrator. Delegate work to one or more worker agents.

## Instructions

### Delegate to one agent

```bash
dot-agent-deck delegate --to <role-name> --task "Task description with relevant context, file paths, and constraints."
```

### Delegate to multiple agents in parallel

Make **one call per agent** so each gets its own task description:

```bash
dot-agent-deck delegate --to coder --task "Implement the login endpoint..."
dot-agent-deck delegate --to reviewer --task "Review the auth module..."
```

If all agents should receive the **exact same task**, you may combine them:

```bash
dot-agent-deck delegate --to <role1> --to <role2> --task "Same task for all."
```

### Signal orchestration complete

When all work is done and you are satisfied with the results:

```bash
dot-agent-deck work-done --done --task "Final summary of what was accomplished across all agents."
```

## Rules

- Always include specific file paths, issue numbers, and other references in your `--task` description.
- Do NOT include full file contents. Workers can read files directly.
- The `--task` value should be a single string. Use quotes to wrap multi-sentence descriptions.
- The `--to` flag accepts role names from your orchestration config (e.g., `coder`, `reviewer`, `auditor`).
- Wait for worker results before delegating follow-up work that depends on their output.
