---
name: dispatch-shape
description: Choose the shape of a unit you are about to dispatch in this repo — one agent (`--single`) or a team (`--orchestration '<name>'`) — from divisibility criteria instead of asking, and report the shape you chose with a one-line reason. Use whenever you are about to run `dot-agent-deck dispatch` in this repo for any reason — an ad-hoc "start X as a separate line of work" in a dispatcher pane, `/issue-queue`, or any other skill that dispatches. It is the maintainer's standing answer to the dispatcher prompt's shape question for this repo; the user's own word in the conversation still wins, and `/prd-queue` keeps its own per-PRD question.
user-invocable: true
---

# Choose a dispatched unit's shape

A unit starts either as **one agent** or as a **multi-role orchestration**. In this repo, **choose it yourself, from the criteria below, and say which you chose and why.** Do not ask per unit, and do not fall back to a default without applying the test.

This skill is where that decision is defined for **every** dispatch made in this repo — an ad-hoc request in a dispatcher pane ("start X as a separate line of work"), `/issue-queue`, or any other skill that dispatches — except the skills named under [Where this skill does not decide the shape](#where-this-skill-does-not-decide-the-shape), which carry a shape step of their own.

## This is the standing answer to the dispatcher prompt's question

Dispatcher mode seeds every dispatcher pane with `DISPATCHER_SEED_PROMPT` (`src/authoring_seeds.rs`), whose section "Choosing the shape — ASK, do not guess" tells you to show the user `--list-targets` and ask which shape they want, once per unit. [`docs/dispatcher-mode.md`](../../../docs/dispatcher-mode.md) carries the same contract for users. You will read both in the same context you are reading this in, so the conflict needs resolving explicitly rather than by whichever you read last.

**The same prompt also says: "One answer can cover several units when the user gives one — take it and stop asking."** This skill is that answer, given in advance by the maintainer for this repo: **the shape of a unit dispatched here is chosen by the criteria below, and the agent reports which shape it chose and why.** So applying the criteria is not the guess the prompt forbids — it is the user's answer, taken, with the question already asked and settled. What the prompt forbids is assuming an answer nobody gave; this one was given.

**The prompt and the doc are not wrong, and this skill does not change them.** They are compiled into the binary and published for every user of dot-agent-deck, in any repo, and they deliberately teach how `dispatch` works rather than how to organise work (PRD #220; `dispatcher_seed_teaches_mechanics_not_work_methodology` in `src/ui.rs` fails if named work-methodology phrases creep back into the seed). The criteria below are specific to this repo — they name this repo's orchestrations — so the product-wide default stays "ask", and this repo's answer lives here. Do not edit either to match this skill.

**The user's word in the conversation still wins.** If they name a shape — for one unit or for a whole batch — take it and stop applying the criteria to those units. A standing answer is a default the user set; a fresh one replaces it.

## The criterion is DIVISIBILITY, not size

A large change confined to one function is a single agent; a medium one spread across separate modules with a decision to argue may be a team. Asking "how big is this?" produces teams on hard problems that do not divide, which is the failure this criterion guards against.

Take **`--single`** when any of these holds:

- the change is confined to **one function, one file, or one tightly-coupled pair** — two agents would collide in the same code;
- it is **one decision to argue** plus its implementation (a policy question, a trade-off, a classification), however subtle;
- it is **mechanical across many call sites** — a sweep is serial work, and splitting it makes the sites inconsistent;
- the task names the fix, or an existing helper/pattern in the tree is the answer.

Take **`--orchestration 'mixed'`** only when the work genuinely splits:

- it touches **separate modules or components** that can progress independently (e.g. both socket paths *and* the hook-endpoint writers *and* a permission helper);
- it carries a **design or transition decision plus implementation plus its own verification**, each substantial;
- it is a **PRD or a user-facing feature** with milestones rather than a defect;
- independent review inside the unit would genuinely catch something — not merely "this feels big".

**When the two readings are close, take `--single`.** A team in one file produces internal conflicts and a longer path to the same diff; a single agent on a divisible task merely takes longer. The failure modes are not symmetric.

## Per unit, never once for the batch

**Apply the criteria to each unit on its own**, including when several are dispatched together. On the 2026-08-24 `/issue-queue` batch: #669 is an `lstat` guard of roughly ten lines in one function, with a reference implementation already sitting on a fork; #668 is an audit of every harness spawn path #661 does not reach, plus a reaping mechanism and its coverage. Those are not the same shape, and two or three tasks off this repo's backlog routinely mix kinds. So applying one shape across a batch is wrong whether it comes from your own shortcut or from reading the criteria once — the only batch-wide shape that holds is one the user states.

## Worked examples

From the 2026-09-19 `/issue-queue` loop. `--single`: a mixed-separator path fix and a log-path default (two one-function fixes, bundled); ~23 tracing call sites needing the same escape helper (mechanical sweep); a delegate readiness race (one decision, one seam); a desktop pane's staleness affordance (one product call). `--orchestration 'mixed'`: a voice-control PRD (new user-facing feature with milestones); a product website (design exploration, build, content, publish); and the `/tmp` endpoint squat, which moves two socket paths *and* the hook-endpoint writers *and* needs a transition strategy plus a versioning decision.

## Record the choice with a one-line reason

**Tell the user the shape you chose for each unit, with the one-line reason** — the criterion that produced it, e.g. "`--single`: confined to one function". Say it when you dispatch and again wherever you report where the work went. A user who disagrees needs to see the criterion that produced the choice, not have to infer it, and that visibility is what makes a standing answer acceptable in place of a question.

## Mechanics

**Pass the matching flag explicitly on every dispatch** (`--single` or `--orchestration '<name>'`). With neither, the shape falls back to whatever the repo's config implies, which is a guess even when it happens to match.

```bash
dot-agent-deck dispatch --list-targets
```

Run that **once** — it is a read-only daemon round-trip and its answer describes the repo, not the unit — to learn which orchestrations exist before naming one.

**If `--list-targets` errors**, the message says which case it is:

- `DOT_AGENT_DECK_PANE_ID environment variable not set` means nothing can be dispatched from here at all. `dot-agent-deck dispatch` reads that variable and exits `FAILURE` without it, and the check runs **before** the `--list-targets` branch (`src/main.rs`, the `Commands::Dispatch` arm), so outside a managed deck pane the dispatch and the shape query both fail. Say so and stop; there is no shape to choose.
- `the daemon did not answer list-targets` means no daemon or an older build. You still have the criteria, and `--single` is a safe shape for anything they select — so **dispatch `--single` and say that the orchestration list was unavailable**, rather than stalling. Only take it to the user when the criteria select a team and you cannot confirm the orchestration's name, since `--orchestration` needs one.

## Which orchestration — `mixed` by default, and the provider is a SESSION property

Since issue #705 this repo defines **three** orchestrations rather than one: `mixed`, `anthropic` and `GPT`. They run the identical six roles with the identical prompts; only which agent each role launches differs.

**Keep shape and provider separate — they are different kinds of decision:**

- **Shape** (single vs team) is a property of **the work** — is it divisible? You decide it per unit, from the criteria above.
- **Provider** (`mixed` / `anthropic` / `GPT`) is a property of **the session** — which credits are healthy today, which stack the user wants exercised. It does not vary with the task at all.

**Default to `mixed` and do not ask** — wherever this skill decides the shape; the skills listed at the end keep their own provider step — because it is the repo's default and exercises the most providers. Say which you used. Re-ask only if the user raises it, or if a dispatch fails on that provider's credentials — a credential failure is a session fact, so carry the new answer forward to every later unit rather than re-deciding each time.

**Pass the name explicitly, always: `--orchestration 'mixed'`, never a bare `--orchestration=`.** The bare form opens whichever orchestration the repo declares as its default, which is currently `mixed` (`default = true` in `.dot-agent-deck.toml`) — a fact about the config file, not a choice the user made in this conversation. `--list-targets` shows which one that is with a `[default]` marker; that marker is there to inform the question, not to answer it.

If the user has no preference, say which one you are taking and why (`mixed` is the default and exercises the most providers) rather than silently omitting the flag.

## Where this skill does not decide the shape

Two dispatching skills in this repo carry a shape step of their own, and **inside them their own steps govern — the shape and which orchestration to name — not this skill**, so neither the criteria nor the `mixed` default above applies there:

- **`/prd-queue`** asks the shape once per PRD (its step 7), because there the shape also decides which of two task documents its step 8 writes — `/prd-full` for a single agent, the orchestrator role template for a team. It also asks the provider once per session, the first time a PRD wants a team (the same step), rather than defaulting to `mixed`. Issue #1425 left it unchanged on purpose; whether it should use this skill instead is an open question, not a settled one.
- **`/pr-review-queue`** asks the shape once per PR (its step 2b), showing the `--list-targets` output, so the answer names the orchestration too. Issue #1425 did not revisit it.

Everywhere else in this repo — an ad-hoc dispatch, `/issue-queue`, or a skill written later without a shape step of its own — this skill decides.
