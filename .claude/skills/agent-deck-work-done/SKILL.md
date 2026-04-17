---
name: work-done
description: "Signal that you have completed your current task. Notifies the orchestration daemon with a structured summary of your work."
user-invocable: true
---

# Work Done

You have completed your current task. Summarize your work and signal completion via the CLI.

## Instructions

Compose a concise summary of what you accomplished, then run:

```bash
dot-agent-deck work-done --task "Your summary here. Include file paths, outcomes, and anything the orchestrator needs to know."
```

## Rules

- Always include specific file paths, issue numbers, and other references in your `--task` summary.
- Do NOT include full file contents in the summary. The orchestrator can read files directly.
- The `--task` value should be a single string. Use quotes to wrap multi-sentence summaries.
- Do NOT use the `delegate` command. Only the orchestrator delegates work.
