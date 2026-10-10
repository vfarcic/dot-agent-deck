# Agent question payloads (PRD #1542)

Hook, plugin-bus and extension payloads that raise a question, used by the `question/detect/*` and `question/hold/*` tests. Each was captured from a real agent during PRD #1542's M1 and follow-up measurements unless this list says otherwise; absolute paths are replaced with `/work/…` and transcript paths, working directories and scratch-directory names are dropped.

| File | Agent and version | Source |
| --- | --- | --- |
| `claude-permission-bash.json` | Claude Code 2.1.289, `PermissionRequest` for `Bash` | captured (M1) |
| `claude-permission-write.json` | Claude Code 2.1.289, `PermissionRequest` for `Write` | captured (follow-up b) |
| `claude-permission-plan.json` | Claude Code 2.1.289, `PermissionRequest` for `ExitPlanMode` | captured (follow-up b) |
| `claude-ask-user-question-form.json` | Claude Code 2.1.289, `PermissionRequest` for a two-question `AskUserQuestion`, the second `multiSelect` | **reconstructed** in the shape follow-up b observed (the single-question capture's fields, two questions) — the two-question capture was not kept |
| `codex-request-user-input.json` | Codex 0.160.0, `PreToolUse` for a two-question `request_user_input` | captured (follow-up c) |
| `codex-permission-request.json` | Codex 0.160.0, `PermissionRequest` for `Bash` | **reconstructed** from the fields follow-up c lists (`turn_id`, `tool_name`, `tool_input {command, description}`, `model`, `permission_mode`) |
| `opencode-question-asked.json` | OpenCode 1.18.34, `question.asked` properties | captured (follow-up d) |
| `opencode-permission-asked.json` | OpenCode 1.18.34, `permission.asked` properties | captured (follow-up d) |
