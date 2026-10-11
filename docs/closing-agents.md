# Closing Agents

`dot-agent-deck close` stops agents from the command line: a unit a [dispatcher](dispatcher-mode.md) started, one agent, or a whole [orchestration](orchestration.md). It does what closing a card or a tab does in the TUI or the desktop app, and it works from a shell or from a dispatcher agent, so a dispatcher can tidy up the units it started when you ask it to.

Closing a dispatched unit also removes its worktree when that worktree has no uncommitted changes. The unit's branch is always kept.

## Close from a shell

Run `close` from a terminal that is not one of the deck's panes. Name the units by the name they were dispatched under:

```bash
dot-agent-deck close fix-auth-bug
dot-agent-deck close fix-auth-bug verify-pr-1201
```

To see first what you could close, list everything:

```bash
dot-agent-deck close --all
```

`--all` on its own closes nothing. It prints every unit with its unit id, its panes and their statuses, whether it has reported back, its worktree and branch, and the dispatcher that started it. When the list is what you want closed, run:

```bash
dot-agent-deck close --all --yes
```

That lists the units again and then closes exactly the units in that list, by their unit ids, so a unit started in the meantime is not swept up. A listed unit marked `would be refused` is refused again unless you add `--force`. If more than 256 units are running, `--all` lists the first 256 and says so; close those, then list again.

To close one unit by the id a list printed, use `--unit-id`:

```bash
dot-agent-deck close --unit-id u-3f9a2c71d04e8b56-4
```

**Check it worked:** the output has a `closed:` section naming each unit and each of its panes with `— stopped`, and a `worktrees:` section saying what happened to each worktree. The unit's card (or its tab in the TUI, its **ORCHESTRATION** group in the desktop app) disappears from every client attached to the daemon.

```
closed:
  fix-auth-bug (u-3f9a2c71d04e8b56-4, single)
    pane 7 agent 12 Idle — stopped
worktrees:
  /home/you/src/api-dispatch-fix-auth-bug: removed (branch agent/dispatch-fix-auth-bug kept)
```

Inside a pane the deck started, `close` acts as that pane's agent, not as you, and the agent rules below apply. A plain shell you opened in a deck pane counts as that pane.

## Close from a dispatcher agent

A dispatcher pane knows the `close` verb. Ask it: *"close the units that have reported back"*, or *"close fix-auth-bug"*. It runs `close` by the deck's full path, the same way it runs `dispatch`. If you let the agent run commands through a permission rule, write the rule against the path shown in the dispatcher's pane.

A dispatcher closes units only when you ask; deciding when work is finished stays with you. A standing instruction works too: *"each time a unit reports back and its PR is merged, close it"*.

An agent running `close` is held to these rules, and `--force` does not change any of them:

- It can close only units it dispatched itself. A unit another dispatcher started is refused (`not-your-unit`), and so is any other agent on the deck.
- It cannot close its own pane, or any role of the orchestration it belongs to (`own-pane`).
- Closing a unit does not close the units that unit dispatched in turn. Those are left running, and the output lists them under `left open (dispatched by it)` so you can close them yourself.
- `close --all` from an agent lists only the units that agent may close.

## Choosing what to close

```
dot-agent-deck close <UNIT>... [--dry-run] [--force] [--json]
dot-agent-deck close --unit-id <ID>... [--dry-run] [--force] [--json]
dot-agent-deck close --pane <PANE_ID> [--dry-run] [--force] [--json]
dot-agent-deck close --orchestration-of <PANE_ID> [--dry-run] [--force] [--json]
dot-agent-deck close --all [--yes] [--dry-run] [--force] [--json]
```

| Argument | What it closes |
|---|---|
| `<UNIT>...` | The dispatched units with these names: the name given to `dispatch`. A unit started as an orchestration is closed whole, orchestrator first. |
| `--unit-id <ID>...` | The dispatched units with these unit ids, as `--all` or an `ambiguous` refusal printed them. A unit id names one unit and is never reused, so it cannot reach a different unit that took the same name later. Unit ids do not survive a daemon restart. |
| `--pane <PANE_ID>` | The one agent in this pane. |
| `--orchestration-of <PANE_ID>` | Every role of the orchestration this pane belongs to, orchestrator first. |
| `--all` | Nothing: it lists every unit you may close. |
| `--all --yes` | The units `--all` listed. |
| `--dry-run` | Nothing: it shows what would be closed and what would be refused. |
| `--force` | Also closes a unit that has not reported back, an agent that is busy, and one role of a running orchestration (see the next section). |
| `--json` | Prints the report as JSON instead of text. |

Pane ids appear in the `--all` list and in `dot-agent-deck daemon status`. There is no pattern or wildcard selector; name what you mean, or use `--all` and read the list. One command names at most 256 units.

## What is refused, and why

Without a terminal you cannot be shown a confirmation dialog, so `close` refuses the cases a dialog would make you think twice about. Each refusal names the reason. A refused unit is left exactly as it was, including every role of a refused orchestration; the other units named in the same command are still closed, and the command exits `1`.

| Reason | What it means | What to do |
|---|---|---|
| `not-reported` | The unit has not reported back with `work-done --done`. It may still be working. | Wait for its report, or check its card. Pass `--force` to close it anyway. |
| `busy` | One of the agents is Thinking, Working, Compacting, waiting for input (**Needs Input**) or Blocked. | Let it finish its turn, or answer it. Pass `--force` to close it anyway. |
| `strands-orchestration` | `--pane` named one role of a running orchestration, which would leave the rest of the team without it. | Close the whole team with `--orchestration-of <PANE_ID>`, or pass `--force`. |
| `not-your-unit` | An agent asked to close something it did not dispatch. | Close it yourself from a shell, or ask the dispatcher that started it. |
| `own-pane` | An agent asked to close itself or its own team. | Close it from a shell or a client. |
| `not-attested` / `superseded` | The request came from a pane the deck cannot confirm, or from an agent that has since been replaced in its pane. | Run `close` from the agent now in that pane, or from a shell. |
| `unknown-unit` | No running unit has that name. Units are not remembered after the daemon restarts. | Check the name with `close --all`. After a restart, see [Worktrees](#worktrees). |
| `already-ended` | The unit has already been closed or has ended. | Nothing to close. |
| `ambiguous` | Units with that name are running in more than one repository. The refusal lists each one with its unit id, worktree and panes. | Close the one you mean with `--unit-id` and its id from that list. |
| `unknown-pane` | No agent is in that pane. | Check the id with `daemon status`. |
| `not-an-orchestration` | `--orchestration-of` named a pane that is not part of an orchestration. | Use `--pane`. |
| `selector-too-large` | The command named more than 256 units, or a name or id far longer than any real one. Nothing was closed. | Close them in smaller batches. |

`--force` overrides `not-reported`, `busy` and `strands-orchestration` only. A forced close says so in its output, for example `(forced: was Working)`.

**Has it reported back** is the check that works the same for every agent: it depends only on the unit running `work-done --done`. The `busy` check depends on the status each agent reports, and some report less than others:

- **Pi** does not report **Needs Input** or **Blocked**, so a Pi agent at a prompt or out of credit is not refused as busy.
- **Devin** does not report **Blocked** or **Error**.
- **Codex** reports its status fully when the deck's Codex hooks are trusted. When they are not (see [Codex events not showing](troubleshooting.md#codex-events-not-showing)), a Codex agent reads **Idle** once its screen has been still for about three seconds, including while a permission prompt waits for an answer, so it is not refused as busy.

An agent whose status the deck has not heard yet is not refused as busy either. Read the `--dry-run` output, or the agent's card, when it matters.

## Worktrees

When the last agent of a dispatched unit is closed, whether you named the unit, its pane or its orchestration, the deck deals with the unit's worktree the same way closing its card does, and `close` waits for the result and reports it. Closing one role of an orchestration with `--pane` while other roles keep running leaves the shared worktree alone:

| Output | What happened |
|---|---|
| `removed (branch … kept)` | The worktree had no uncommitted changes and was removed. Its branch is kept, because it may hold committed work. |
| `kept: uncommitted changes` | The worktree is left on disk so the work can be recovered. |
| `kept: could not check for uncommitted changes` | The deck could not tell whether the worktree was clean, so it kept it. |
| `kept: removing it failed` | Removing the worktree failed; it is still on disk. |
| `kept: still in use` | Something else was using the directory at the time. |
| `cleanup still running: …` | The agents are stopped, but the removal had not finished within about 30 seconds. Check with `dot-agent-deck worktree list`. |
| `not recorded by the daemon …` | The daemon has no record of this worktree, for example because it restarted after the unit started. |

To clean up worktrees the deck no longer knows about, such as after a daemon restart, use `dot-agent-deck worktree list` and `dot-agent-deck worktree reclaim` ([Dispatcher Mode → Finish up](dispatcher-mode.md#finish-up)). A kept branch blocks dispatching the same name again until you delete it (`git branch -D agent/dispatch-<name>`).

## Exit status and JSON

| Exit status | Meaning |
|---|---|
| `0` | Everything named was closed, or listed. |
| `1` | Something was refused, a close failed or was only partial, or the command was run with an incomplete deck environment (see below). |
| `2` | The running daemon is too old for `close`. Nothing was closed. |
| `3` | No daemon is running. Nothing was closed. |

`--json` prints the daemon's report with four lists added for scripts: `closed`, `refused`, `listed` and `worktrees`, plus `exit_code`. Each entry names the unit in `name` and gives its `unit_id`. In `closed`, `refused` and `worktrees`, a unit id the deck does not know is `null`, and a refusal of the whole command, rather than of one unit, is an entry in `refused` whose `name` and `unit_id` are both `null`. Entries in `listed` always carry `panes`, and carry `name`, `unit_id`, `reported`, `completed_at_ms`, `worktree`, `branch` and `clone` only when there is something to report, leaving each one out rather than setting it to `null`: `completed_at_ms`, for example, is missing for a unit that has not finished. With `--all --yes`, the list that was closed from is under `preview`. `truncated` is `true` when `--all` listed only the first 256 units.

A script that closes the units whose pull requests merged can join `close --all --json` with `dot-agent-deck worktree list --json`, which reports each worktree's PR state, on the branch, and close each selected unit with `close --unit-id` and the `unit_id` from `listed`.

## Closing in the TUI and the desktop app

The TUI and the desktop app close the same things and clean up worktrees the same way, with a confirmation dialog instead of the refusals above.

**TUI:** select the card on the dashboard, or go to the orchestration's tab, and press `Ctrl+W` in command mode, or click the card's `[Close]` button or the tab's `[×]`. Choose **Close** in the confirmation. If the unit's worktree has uncommitted changes, the confirmation says it will be kept and names it, and the status line reports what happened after the close. See [Keyboard Shortcuts](keyboard-shortcuts.md).

**Desktop:** press the stop control on the agent's row and then **Close agent**, or **Close** on an orchestration's group header and then **Close all N roles**. See [Desktop App → Dashboard](desktop/dashboard.md#closing-agents-and-orchestrations). The desktop app does not currently say when a worktree was kept; check with `dot-agent-deck worktree list`.

A unit closed with `close` disappears from both clients, and a unit closed in a client is gone from `close --all`.

## When something goes wrong

| Message | Cause | What to do |
|---|---|---|
| `the running daemon (…) is too old for close; nothing was closed` | The daemon was started by an older version of the deck. | Restart it onto this version with `dot-agent-deck daemon restart`, or close from the TUI or the desktop app. |
| `no daemon is running …; nothing was closed` | No daemon is running for this user. `close` never starts one. | Nothing is running, so there is nothing to close. |
| `close: …` naming `DOT_AGENT_DECK_PANE_ID`, `DOT_AGENT_DECK_AGENT_ID` or `DOT_AGENT_DECK_PANE_CAPABILITY` | Some of the variables the deck sets in its panes are present and some are missing or empty, so `close` cannot tell whether a person or an agent is asking. It refuses rather than guess. | Run it from a terminal outside the deck, where none of them is set, or from the agent's own pane with the environment the deck gave it. |
| `PARTIALLY: still running: …` | Some agents of the unit stopped and some did not. | Close the survivors with `--pane`, or from a client. |
| An agent's `close --all` lists nothing | That agent dispatched no units that are still running. | Close from a shell instead. |

## What the agent rules protect against

The agent rules stop an agent from closing the wrong thing by mistake: a unit another dispatcher started, its own team, a pane whose agent has been replaced, or a unit that has not finished. They are not a lock. Any program running as your user account can stop the deck's agents by other means, and the deck does not try to prevent that.

## See also

- [Dispatcher Mode](dispatcher-mode.md): start units and hear back from them
- [Orchestration](orchestration.md): the teams a unit can start as
- [Session Management](session-management.md): what each status means
