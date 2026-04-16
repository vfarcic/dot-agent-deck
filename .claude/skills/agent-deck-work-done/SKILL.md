---
name: work-done
description: "Signal that you have completed your current task. Writes a structured summary of your work to .dot-agent-deck/work-done.md for the next agent or person picking up this work."
user-invocable: true
---

# Work Done

You have completed your current task. Write a summary of what you accomplished so the next person or agent picking up this work has full context.

## Instructions

Delete `.dot-agent-deck/work-done.md` if it exists, then create it fresh (relative to the project root).

Write down everything relevant about what you did, your findings, and anything the next agent or person should know. Structure the content however makes sense for the work you performed. Be specific about outcomes, not just activities.

Include references (file paths, URLs, issue numbers, etc.) for everything you mention. The next agent needs to be able to find and read the actual sources, not just take your word for it.

## Rules

- Do NOT include the full content of files in the summary. The next agent can read them directly — just provide the paths.
- Write in plain markdown with no YAML frontmatter or special formatting.
