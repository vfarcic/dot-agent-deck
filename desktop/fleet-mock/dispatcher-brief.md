# Compose your fleet view (PRD 2557 mock-up, round 3)

This is a design test for PRD 2557, the fleet view. **Compose only: do not act on anything.** Do not merge, close, dispatch, push or change any agent's work because of this request, and do not treat anything in the view you compose as a decision the maintainer made. Your normal work carries on unchanged after this.

## What the fleet view is

When the maintainer opens the desktop app's fleet view, the deck asks the top-level agent (you) for its view, and draws whatever you send back. You decide everything about the screen: what goes first, how it is organised, what is a picture and what is text, what is left out. There are no predefined sections. The view does not have to be organised by agent: agents are one of the ways you get work done, alongside doing it yourself or spawning a new unit, so show them only where that helps the maintainer understand something.

The view is your picture of the work you look after: everything you can see, whether or not an agent is behind it (pull requests, CI, issues, units, things you noticed). It has two jobs:

1. **An overview the maintainer can take in at a glance**, whatever the work is. Prefer pictures where they say it faster than words: a board, a chart, coloured chips, progress, a timeline. Use text where a picture cannot say it.
2. **Easy actions where something waits on the maintainer.** Every action is text sent back to you. Choose the form that fits the decision: buttons for the answers you can predict, a set of choices or checkboxes when several things can be picked (for example "tick the units to close" with a "Close {/toClose}" button), a text box for anything you cannot predict. For work in progress, show its state and offer no actions, unless something in it looks wrong; then say what looks wrong and offer a way to step in. Whether you would do a thing yourself or spawn an agent for it is not part of the choice you offer.

Pull request and issue state goes in `DeckRef` (the deck checks it itself); agents go in `AgentRef`. You may ask your units questions before composing if you think the answer is worth interrupting them for, but compose from what you know rather than waiting long.

## How

1. Read the catalog: `~/code/dot-agent-deck-dispatch-prd-2557-mockups/desktop/fleet-mock/fleet-catalog.md`. Use only its components and its A2UI v1.0 format. Your `PromptButton`s go to yourself (`agent`: your name as the deck shows it, `dispatcher`).
2. Write the JSON array of A2UI messages to `~/code/dot-agent-deck-dispatch-prd-2557-mockups/desktop/src/data/fleetMock.dispatcherView.json`, with `/composedAt` set to the time you compose it.
3. Check it: `python3 -I ~/code/dot-agent-deck-dispatch-prd-2557-mockups/desktop/fleet-mock/validate-view.py ~/code/dot-agent-deck-dispatch-prd-2557-mockups/desktop/src/data/fleetMock.dispatcherView.json`. Fix what it reports and run it again until it prints `ok`.
4. Reply with one line: "fleet view written", plus anything you want the maintainer to know about how you chose to lay it out.

The view will be drawn by the desktop app's own components and posted, with the maintainer's agreement, on the public issue #2557, so write nothing in it that should not be public (no home-directory paths, no secrets).
