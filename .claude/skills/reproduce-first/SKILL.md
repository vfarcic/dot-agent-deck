---
name: reproduce-first
description: 'Reproduce first, then fix — turn a reported defect into a failing test, fix it, and confirm that same test goes green. Use whenever the user describes the software behaving differently from what they expected or intended, however they phrase it: a complaint, a neutral observation, a question about whether something is meant to work that way, or an aside that something works "except for" one detail. It applies to a report that arrives mid-task about unrelated work, and to a symptom mentioned in passing. Trigger on the situation, not on any particular wording. Invoke it BEFORE reading code to diagnose the cause and before proposing any fix.'
user-invocable: true
---

# Reproduce a reported bug as a failing test, then fix it

## When to use this

The user has reported that something is broken. That includes the quiet forms: "it works except for…", "I have to do X twice", "it stopped doing Y", "is it supposed to…?".

Do NOT use it for: a request for a new feature, a question about how something works, a change to docs or other text, a refactor, or a bug you found yourself while already mid-task with a test already failing for that reason.

## The order is the whole point

**The first deliverable is a test that fails for the user's reason. Not a fix.** Then fix it, and watch that same test pass.

Do not reverse the order, and do not skip to the fix because the cause looks obvious. A fix whose test was written afterwards tends to assert what the code now does rather than what the user asked for. "The cause is obvious" is the single most reliable predictor that it is not — see the case studies below, where two confident diagnoses were both wrong and only the control runs exposed it.

## Process

1. **Restate the symptom as the user sees it.** At their altitude: a card that stays on screen, a tab that never appears, a name that reads wrong. Not "the registry entry is stale".

2. **Find the test that already covers this surface, and EXTEND it.** Bias order: extend an existing test > modify an existing test > write a new one. Only add a brand-new `#[spec]` id when no catalog entry covers the surface at all — search `tests/CATALOG.md` for the area before assuming there isn't one.

   Most follow-up reports are the SAME behaviour under a different configuration (a real agent instead of a stand-in, an orchestration instead of a single agent, a command the deck cannot infer an agent type from). That is a case for widening the existing test's coverage to the configuration that actually broke — not a parallel test duplicating 90% of the setup. A new spec id is justified when the mechanism genuinely differs, not when the inputs do.

   This matters more at L2 than anywhere else: every PTY test spawns real binaries, and a suite of near-duplicate e2e tests is slow for everyone and harder to diagnose than one test that names its cases.

3. **Assert at the user's altitude.** The user-visible outcome, not an adjacent artefact — a file on disk, a log line, or a registry entry can all be correct while the screen is wrong. If they said "no orchestration tab appeared", the assertion is a tab with live role cards.

4. **Run it and confirm it fails FOR THEIR REASON.** A test that fails for a setup error (a stub that broke an unrelated path, a form field that was pre-filled, an ellipsized card title) is not a reproduction. Read the failure message and check it describes their symptom. Fix the harness problem and re-run until the red is the right red. If no test you can build fails for their reason, go no further: see "When it does not reproduce" below.

5. **Add a control that isolates the cause.** The nearest thing that should still work — the same close on a card with no worktree, the same dispatch with a fast `git` — proves the failure is attributable to what you think it is. Without a control you cannot tell "this path is broken" from "this whole feature is broken".

6. **Fix it.**

7. **Watch the same test go green**, then prove each fix is load-bearing: revert one change at a time and confirm the test goes red again. If reverting a change leaves the test green, that change is not part of the fix — take it out or find what it is really doing.

8. **Run the wider tier** before reporting done: `cargo xtask affected-checks --run` (for a code change that is CLAUDE.md rule 2's fmt and clippy commands and rule 5's `cargo test-fast`), plus the tests covering what you touched. There is no full-tier obligation before the PR — per CLAUDE.md rule 5, lane 1 runs in CI on every PR, so read that run rather than reproducing it. If your reproduction is an e2e test, run it and its module by filter (`cargo test-e2e <filter>`, or `cargo test-e2e-live <filter>` when the test reaches a real agent); do not run either alias unfiltered. A lane-2 test is worth running deliberately: nothing in CI runs one, so if you do not, nobody does.

## The traps that have actually cost time here

**Prove the test can fail.** An assertion never observed failing is not evidence. This is the cheapest step and it has caught a vacuous assertion repeatedly: a stream-based wait that could never match redrawn chrome; a `wait_until_grid` capped at the harness's 10s `WAIT_TIMEOUT` that silently shortened an intended 60s wait; and an `AgentRecord.live` check that was `Some(Idle)` for every role within 1.5s of the spawn, before a byte had reached any of those PTYs.

**Prefer their configuration over a convenient stand-in.** `cat` roles and print-mode agents prove the plumbing and hide everything else: they cannot tell an agent from a `$SHELL`, and they never read an orchestrator-context file, so both of those defects shipped green. Where a stand-in is genuinely necessary for cost, say so in the test, name what it stands in for, and add one real-config case beside it.

**A stand-in must be narrow.** A `git` stub that slept on every `status` also hit the deck's own pane-creation path, which has its own 5s budget — the pane never came up and the test failed before reaching what it was about. Key the stand-in to the exact invocation under test.

**Reproduce before diagnosing, and diagnose on the reporter's machine, not yours.** Environment-shaped bugs do not travel: a whole diagnosis was once built from this server's process table while the user was reporting from a laptop. Ask for the artefacts — the message the pane printed, `~/.local/state/dot-agent-deck/deck.log`, `command -v` — instead of inferring them locally.

## When it does not reproduce: stop, and hand the decision back

**If no test fails for the user's reason, do not write the fix, and do not open a pull request.** "The user's reason" means their symptom, through the path they used, in their configuration as far as you can build it. A plausible cause that nobody has seen happen is not a reason to change code: a fix for it may handle nothing the user hits, and once it merges, the issue looks dealt with while the user still has the bug.

Stop and report instead, to whoever gave you the work (the user in a conversation; the dispatcher, through `work-done`, in a dispatched unit). Say:

- what you ran: the configurations, the agent releases, how long and how many attempts;
- what that showed, and what it ruled out;
- what you could not cover, and what is missing to reproduce it: the reporter's artefacts, a platform, a release, a scale;
- the options, with the one you recommend. Usually some of: ask the reporter for specific evidence (name exactly what to capture), add diagnostics so the next occurrence shows the cause, reproduce a narrower cause that the code shows is possible, build a fallback that limits the damage whatever the cause, or close the issue as not reproducible.

Then wait for the decision: in a conversation, the person's; a dispatched unit ends after its `work-done` report, and the decision is made outside it. A fallback is a legitimate outcome, but someone has to choose it knowing the cause is still unknown; don't build one by default.

**Forcing the suspected cause is not a reproduction.** A test-only switch or stub that makes the guessed failure happen proves the fix handles that guess, not that the guess is what the user hit. Issue #2385 is the example: a re-wake fallback (PR #2501) came with a test that held mail back from Claude Code's wake-up through a test-only switch. Two hours of real-agent soak runs afterwards woke every message, and the cause that did reproduce was a different path: a turn that ended on an API error left no wake-up armed.

**"I could not reproduce this" is a complete and useful report.** Presenting a fix for an unreproduced bug as verified is not.

## Case studies from this repo

**Two stacked defects, one symptom** (`dispatch/close/001`). Reported: closing a dispatched agent left its card behind; a second close removed it. The reporter's guess was that worktree removal blocked the close. The first diagnosis (mine) was a client-side timeout. A control run with the slowness removed **still failed** — which exposed a different, primary defect underneath: a daemon-spawned card has no local pane until focused, so `close_pane` returned "not found", the card was preserved by policy, and the agent kept running. Only after fixing that did the reporter's timeout theory become the *remaining* cause. Both were real; reverting either fix alone turns the test red. Neither would have been found by reading code.

**The assertion that proved nothing** (`orchestration/dispatch/002`). The first version passed in 1.5 seconds — impossibly fast for three agent cold boots. It was asserting a field the daemon populates at spawn time. Rewritten to assert what the user looks at (every role named on its own card), it immediately caught a real defect: dispatched role cards were labelled with session UUIDs instead of the role names in the toml.

## Related

- `CONTRIBUTING.md` — the team-facing statement of this norm, and the TDD loop commands.
- `CLAUDE.md` rule 4 (which test tier a change needs) and rule 5 (fast tier per task, plus the tests covering the change; lane 1 runs in CI, and lane 2 runs nowhere but your machine).
- `tests/CATALOG.md` — every test's entry records what it does *not* assert; add yours there.
