## The deck checks Codex's input box before it types its doorbell, on Linux

Once you had typed anything into a Codex agent's pane, the deck stopped typing its doorbell into that Codex until it restarted, and its messages waited for your next prompt. Now, for Codex running on Linux and started by the deck in its pane, directly (its command starts with `codex`) or through a launcher such as `devbox run …`, the deck checks the input box first and rings once a check finds it empty, so you can clear the box any way you like. It checks only while Codex shows its empty input box with its grey placeholder; while the box shows anything, the card says `there may be an unsent draft. Send it or clear it.` What you type during a check reaches Codex in order.

To check the box, the deck sets that Codex's `VISUAL` and `EDITOR` to a small helper of its own, which still opens your own editor when you press `Ctrl+G`. In a Codex started directly in its pane, the commands Codex runs keep the `VISUAL` and `EDITOR` you set.

What to know:

- After `/cd`, `/worktree` or `/resume` moves Codex into another project, close its pane and start Codex again before you rely on the doorbell: until then the deck checks with the first project's key settings, and if the new project gives `Ctrl+G` another action, the deck's check does that action instead.
- If something sets `VISUAL` or `EDITOR` again before Codex starts, such as your shell's start-up file (zsh's `~/.zshenv`, fish's `config.fish`) or a launcher like `VISUAL=nvim codex`, the deck doesn't check that Codex's box and types nothing into it; the card says `the deck can't check this agent's input box. Check its pane.` Set them where only an interactive shell reads them (zsh's `~/.zshrc`) to have the box checked.
- A launcher that passes Codex its own `-c` options or a profile, a change to Codex's settings files while it runs, and a project whose `.codex/config.toml` sets key bindings leave the deck unable to check the box; the card then says `the deck can't check this agent's input box. Check its pane.` and a prompt hands the agent its messages.
- When Codex's home (`~/.codex` or `CODEX_HOME`) is inside a folder Codex may write to, such as the project it works in or `/tmp`, the deck can't check the box: Codex shows `Failed to open editor` once for the deck's check and the card keeps saying `there may be an unsent draft`. Keep Codex's home outside the folders Codex works in.
- Codex writes a few lines into its `config.toml` on its first starts in a new Codex home. The deck expects that; there is nothing to configure.
- Codex started through `sh -c`, or on macOS, isn't checked: once you type into its pane, its messages wait until it restarts, as before. `dot-agent-deck config set doorbell.codex false` turns both the doorbell and the check off for Codex.
- Every Codex start writes a `doorbell off … unknown_release` line to the deck log before the deck has read Codex's version; that line is normal.

The TUI and the desktop app show the same card words. See [Orchestration → How the deck checks Codex's input box](https://agent-deck.devopstoolkit.ai/docs/orchestration.md#how-the-deck-checks-codexs-input-box).
