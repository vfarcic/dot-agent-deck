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
- **The remembered last command** persists for the TUI (in its session file) and is in-memory per deck for the desktop. Not a design preference: the desktop's settings file refuses free text, because a command line is where people put secrets. Named, rather than quietly divergent.
- **Config staleness** differs by mechanism. The TUI reads `DashboardConfig` once at startup, as it always has for `default_command`; the daemon re-reads it per query, so the desktop sees a change immediately and the TUI sees it on next launch.

## When you are writing a PRD

State, for each user-visible capability the PRD adds, whether the other client gets it. If it does not, give the reason and make it one someone can disagree with — "the desktop may be on another machine", "the TUI has no such surface", "the file cannot hold this value" — rather than "out of scope for this PRD". A scope line that reads as a blanket exclusion will be implemented as one.
