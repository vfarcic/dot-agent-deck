**This project already has a configuration: `{dir}/.dot-agent-deck.toml` already exists.** Update it rather than creating one:

- **Read `{dir}/.dot-agent-deck.toml` first**, before proposing anything, and treat what it holds as the user's own work.
- **Propose changes to that file, not a replacement for it.** Show what you would add, change or remove, and why. Preserve everything the user wrote — orchestrations, roles, commands, prompt templates, comments — unless they agree to drop it.
- **Keep what the user wrote exactly as written.** For every orchestration, role, `description`, `command` and `prompt_template` already in the file, keep the user's text word for word: do not replace it with the Role Library's text or with what the rest of this request suggests, do not reword it, and do not move it to another field. The Role Library's descriptions and prompt templates, and the composition advice below, apply only to roles and orchestrations you add. Do not propose changing the user's text just to match the library; if something the user wrote must change for the project to work (a `command` whose binary is not installed, for instance), propose that change as an explicit before/after — the current text, then what you would put instead — and make it only if the user approves it.
- **Write only after the user's approval** of the proposed changes (step 4 below), and then edit the existing file in place. In step 6, say "Config updated!" rather than "Config created!".
{link}
The rest of this request still applies: use it to analyze the project and to decide what to propose.

