## Troubleshooting covers a Codex role stuck at its upgrade prompt

If a Codex role stops at Codex's "a new version is available" prompt and never takes its task, the troubleshooting page now says to add `-c check_for_update_on_startup=false` to the role's `command`.
