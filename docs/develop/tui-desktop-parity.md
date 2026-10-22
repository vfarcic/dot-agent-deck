# TUI and desktop parity

This project has two clients of one daemon: the terminal UI and the desktop GUI. The standing principle is that **they should have parity wherever parity makes sense**, and a difference between them needs a reason rooted in what actually differs — not in which client happened to be built first, and not in which one a given PRD was about.

## The rule, and the test for a legitimate difference

A fact the **deck** owns belongs to every client that can use it. The daemon already serves the things that describe a deck — its default command, its agent registry, its experimental flag, the projects it can resolve — and a new one should be read by both clients unless there is a reason it cannot be.

There is exactly one thing that genuinely differs between the two: **the desktop may be running on a different machine from the deck it is driving; the TUI never is.** It runs on the daemon's host, locally or over `dot-agent-deck connect`'s `ssh -t`. Every honest asymmetry traces back to that, or to a client-specific constraint that can be named.

So when a PRD adds a user-visible capability to one client, the question to answer in the document — not at review time — is: *does the other client want this, and if not, why not?* "This PRD is about the desktop" is not a reason.

## Worked example: `default_dir` (PRD #1223)

PRD #1223 gave the desktop a New agent flow and, with it, a per-deck default directory so the browser opens where work actually happens. The value was put where it belongs: `DashboardConfig.default_dir`, a key in the deck's own host-side config file, beside `default_command`, served to the desktop through `NewAgentOptions`.

It was then read by the desktop alone. The TUI's `Ctrl+n` picker still opened at the TUI process's current directory, and the documentation papered over the gap with a sentence that said the picker "does not read it yet".

The cause was a scope bullet. The PRD's Out of Scope list said "Changing the TUI's picker or form", meaning one narrow thing — the TUI keeps reading its own filesystem rather than moving onto the new daemon-side listing verb, which is correct precisely because the TUI always runs on the daemon's host. Written as a blanket exclusion, it was read as "do not touch the TUI", and a deck-level setting shipped as a desktop-only one.

The fix was small: the TUI's picker now honours `default_dir` through the same validator the daemon uses (`usable_default_dir` — absolute, canonical, a directory, readable; anything else yields nothing), falling back to its launch directory exactly as before. Scheduled Tasks **Add** uses it too, because that is also creating an agent on this deck; **Edit** keeps the row's own working directory, because a directory that schedule already runs in is more specific than any deck default.

**Do this while the setting is new.** Nobody had `default_dir` set, so teaching the second client to honour it changed no existing behaviour. The same change a release later would have moved the ground under anyone who had adopted it.

## Asymmetries that are legitimate, and why

- **Voice** exists only on the desktop. The TUI has no voice surface at all, so there is nothing to keep in parity. See [voice-first design](voice-first-design.md).
- **Directory browsing** goes through the daemon's `ListDirectories` verb for the desktop, and through the filesystem directly for the TUI. This is the "different machine" difference in its purest form: the TUI is always on the host it is browsing. **What each can reach differs too, since issue [#1240](https://github.com/vfarcic/dot-agent-deck/issues/1240):** the desktop browser can show hidden directories, lists symlinked ones by their targets, and asks the deck to filter a directory its 1000-entry cap truncated. The TUI's picker still skips hidden and symlinked entries and has no cap to reach past, since it reads the whole directory itself. The desktop gained those because browsing is its only way to choose a directory now that its typed-path field is gone; the TUI's picker was left alone, which a follow-up could revisit.
- **Config staleness** differs by mechanism. The TUI reads `DashboardConfig` once at startup, as it always has for `default_command`; the daemon re-reads it per query, so the desktop sees a change immediately and the TUI sees it on next launch.

## A documented difference: admitting a daemon a declared contract break away (PRD #1666 part 2)

The two clients refuse a daemon on another attach protocol alike, at connect and on every gated request (`docs/develop/versioning.md`, item 1). They do **not** admit a same-protocol daemon whose declared contract breaks differ the same way, and this is kept as a decision rather than closed:

- **The TUI** decides at startup. Its build-mismatch prompt offers to restart the daemon; declining keeps the running daemon and attaches at equal protocol. Its mid-session gate (`DaemonClient::with_protocol_gate`) then checks the protocol alone, so a same-protocol replacement whose declared breaks differ is admitted without the TUI asking again.
- **The desktop** refuses that daemon at connect and on every gated request, unless the user chooses **Connect anyway**. That choice is one switch for the whole app session: it applies to every deck the app observes and to any later replacement on the same protocol, until the app quits.

The constraint behind the TUI's side is CLAUDE.md rule 12: its cross-version test runs the branch TUI against a previous-release daemon by declining that prompt, so a TUI that refused every declared break would make the required test impossible. Changing it needs a consent-aware design for the TUI (an explicit allowance it can offer, and fresh consent for a later replacement), not an unconditional refusal. Until that exists, the user docs describe each client's behaviour separately rather than claiming they match.

## The restart notice, and where the two clients differ after a deck restart (PRD #1666 part 2)

Both clients drop the cards and panes of agents a deck restart stopped and name them in one notice with the same words (`agents_stopped_by_restart_notice` in the TUI, `agentsStoppedByRestartNotice` in `desktop/src/lib/agentIdentity.ts`), held until dismissed, with later drops joining it (`UiState::restart_notice`; `useRestartNotice`). The surfaces differ as the clients do: the TUI gives it footer rows of its own, lists every name on `a` and dismisses on `c` (both remappable, `restart_notice` / `dismiss_restart_notice`) or with the footer's `[Agents]` / `[Dismiss]`; the desktop shows a corner toast that lists every name and closes on its `×`. The mechanism is in [versioning.md](versioning.md), item 1. Two differences are kept, and the user docs state each:

- **Confirmations that act only on drawn choices are a TUI concern.** The Quit, Close, Stop and configuration confirmations drop input queued when they open and refuse a key that would pick a choice their last frame did not show: the record is read back from the frame as flushed to the terminal, after every write of that draw, footer rows included, so a choice counts only when its marker and whole label are intact, and a button is a click target only when its whole label is intact inside the popup (`record_flushed_confirmation`, `UiState::confirm_choices_drawn`, `refuse_undrawn_choice`). A terminal can be too small to draw a dialog; a desktop window lays its dialogs out itself. Issue [#1799](https://github.com/vfarcic/dot-agent-deck/issues/1799) tracks the TUI dialogs this does not cover yet (Schedules delete, the star prompt, the directory picker).
- **An orchestration started during a reconnect gap gets no tab or role cards in the TUI** (issue [#1800](https://github.com/vfarcic/dot-agent-deck/issues/1800)). The TUI builds a role's tab from the orchestration's surface announcement, which a stream gap loses, and its resync cards dashboard agents it missed (`AppState::card_unseen_agents`) but deliberately skips orchestration roles, because rebuilding a tab needs the render thread's tab machinery. The desktop groups agents from each snapshot's tab membership, so it shows them. `docs/troubleshooting.md` names the gap and the remedy (detach and start the TUI again, which rebuilds tabs from what the deck lists).

## Where a client cannot match: ⌘ chords

CLAUDE.md rule 22 asks that, where a client genuinely cannot match because no channel exists, the user docs name the gap and what the user can do about it. ⌘ chords are the example: the TUI receives one as a chord only from a terminal that passes the Super modifier on, as kitty does under the enhanced keyboard protocol, and in terminals that keep the chord or drop the modifier — by default iTerm2 switches tabs on `⌘←` / `⌘→` and WezTerm sends a bare arrow, and tmux has no Super modifier at all — it never does, so `docs/keyboard-shortcuts.md` says what to set in the terminal or what to press instead.

## Worked example: one editing-shortcut table (PR #1429)

PR #1429 (issue #1422) is the worked example of CLAUDE.md rule 22. It changed both clients to one editing-shortcut table on every platform, `tests/fixtures/editing-shortcuts.json`: the TUI's `editing_chord_bytes` (`src/ui.rs`) and the desktop's `editingSequence` (`desktop/src/lib/terminalKeys.ts`) both translate exactly the chords it lists, and tests on each side read the file, so a change to the table that only one client follows fails the other client's tests. Only the paste key still depends on the platform, and the one difference it could not close — the desktop app on Linux cannot see the Super key, because WebKitGTK does not report it — is named in the user docs.

## When you are writing a PRD

State, for each user-visible capability the PRD adds, whether the other client gets it. If it does not, give the reason and make it one someone can disagree with — "the desktop may be on another machine", "the TUI has no such surface", "the file cannot hold this value" — rather than "out of scope for this PRD". A scope line that reads as a blanket exclusion will be implemented as one.
