**The deck could not check whether this project already has a configuration.** Check `{dir}/.dot-agent-deck.toml` yourself before anything else:

- **If `{dir}/.dot-agent-deck.toml` exists, read it first** and treat what it holds as the user's own work. Propose changes to that file, not a replacement for it, and preserve everything the user wrote — orchestrations, roles, commands, prompt templates, comments — unless they agree to drop it. Write only after the user's approval of the proposed changes (step 4 below), and then edit the existing file in place. In step 6, say "Config updated!" rather than "Config created!".
- **If it is a symbolic link**, edit the file it points to in place, through the link, and keep the link: never replace it with a regular file, remove it or re-point it. If the link points to a file that does not exist, write nothing: tell the user the link is broken, and that they can restore the file it points to or remove the link, and then ask again.
- **If nothing exists at that path**, create the file as described below.

