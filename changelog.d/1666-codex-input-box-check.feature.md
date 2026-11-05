## The deck checks Codex's input box before it types its doorbell, on Linux

Once you had typed anything into a Codex agent's pane, the deck stopped typing its doorbell into that Codex until it restarted, and its messages waited for your next prompt. Now, for Codex running on Linux and started by the deck in its pane, directly (its command starts with `codex`) or through a launcher such as `devbox run …`, the deck checks the input box first and rings once a check finds it empty, so you can clear the box any way you like. It checks only while Codex shows its empty input box with its grey placeholder; while the box shows anything, the card says `there may be an unsent draft. Send it or clear it.` What you type during a check reaches Codex in order.

To check the box, the deck sets that Codex's `VISUAL` and `EDITOR` to a small helper of its own, which still opens your own editor when you press `Ctrl+G`. In a Codex started directly in its pane, the commands Codex runs keep the `VISUAL` and `EDITOR` you set.

What to know:

- After `/cd`, `/worktree` or `/resume` moves Codex into another project, close its pane and start Codex again before you rely on the doorbell: until then the deck checks with the first project's key settings, and if the new project gives `Ctrl+G` another action, the deck's check does that action instead.
- A launcher that passes Codex its own `-c` options or a profile, a change to Codex's settings files while it runs, and a project whose `.codex/config.toml` sets key bindings leave the deck unable to check the box; the card then says `the deck can't check this agent's input box. Check its pane.` and a prompt hands the agent its messages.
- Codex writes a few lines into its `config.toml` on its first starts in a new Codex home. The deck expects that; there is nothing to configure.
- Codex started through `sh -c`, or on macOS, isn't checked: once you type into its pane, its messages wait until it restarts, as before. `dot-agent-deck config set doorbell.codex false` turns both the doorbell and the check off for Codex.
- Every Codex start writes a `doorbell off … unknown_release` line to the deck log before the deck has read Codex's version; that line is normal.

The TUI and the desktop app show the same card words. See [Orchestration → How the deck checks Codex's input box](https://agent-deck.devopstoolkit.ai/docs/orchestration.md#how-the-deck-checks-codexs-input-box).
