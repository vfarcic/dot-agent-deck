# PRD #743: Follow the OS light/dark appearance, with an in-app override

**Status**: In progress — implemented alongside [PRD #803](803-desktop-settings-surface.md) in one PR
**Priority**: Medium
**Created**: 2026-09-02

## Problem Statement

The desktop GUI does not support dark mode and is currently pinned to light. On a Mac in dark mode it is a bright panel among dark windows.

Two lines hold it there. `desktop/index.html:7` declares `<meta name="color-scheme" content="light" />`, and `desktop/src/styles.css:1-29` defines a single light palette on `:root` with no `prefers-color-scheme` block and no `data-theme` hook anywhere in `desktop/src`.

The app should follow the OS-level appearance by default, with an in-app override — Light, Dark, or System — that persists.

## Solution Overview

Three pieces, in the order they have to be built.

1. **A semantic palette that covers the whole app.** The token set on `:root` is good and is not the problem; what is on `:root` today covers roughly a seventh of the colour actually used. Everything else is a hex literal in a rule, and a hex literal cannot follow a theme. This is the bulk of the work and it comes first, with the property that **light mode looks exactly as it does today**.
2. **A dark value for every token**, delivered by `prefers-color-scheme` for the default and a `data-theme` attribute for the override.
3. **The override itself** — Light / Dark / System — persisted in the settings store that PRD #803 builds, rendered in the Appearance section of the settings surface that PRD #803 builds.

Pieces 1 and 2 are this PRD's. Piece 3 is the seam between the two PRDs: #803 owns the store, the sheet and the section registry; #743 owns the Appearance panel that sits in it and everything the choice actually does.

### Why this ships with #803 rather than after it

The two are mutually blocking in practice. This PRD needs somewhere for the override to live, and #803's own issue says the same thing from the other side: it is a container three PRDs need, and a container with no tenant is proven only by its own tests. Shipping them together means the settings surface arrives with a reason to open it, and the container is validated by a real consumer on its first day rather than by the second PRD to touch it.

The cost is one large PR. It is mitigated by staging — see Milestones.

## Scope

### In Scope

- A semantic token palette covering every colour in `desktop/src`, replacing the hex literals now scattered through the rules.
- A dark value for each token, with `prefers-color-scheme: dark` as the default path.
- A `data-theme="light" | "dark"` override on the document root that wins over the media query in both directions.
- An **Appearance** section in #803's settings surface: Light / Dark / System, persisted, applied immediately with no restart.
- `<meta name="color-scheme">` and `<meta name="theme-color">` moved off their hard-coded light values so the webview's own chrome, form controls and scrollbars follow.
- A **guard** that fails the build on a new hex literal outside the token block, so the theme cannot rot by attrition.
- Coverage of the whole surface the app has today: the deck, every sheet, the command palette, the shortcut dialog, the confirmation dialog, the connection banner and every failure state.

### Out of Scope

- **The embedded terminals. They stay dark, exactly as they are today, in both themes.** This is a decision, not an omission — see Technical Approach.
- **PRD #745's agent-overview screen** ([PR #779](https://github.com/vfarcic/dot-agent-deck/pull/779), open as a draft). It is not on this branch, so it cannot be themed here. Deliberately deferred: when #745 merges to `main`, this branch merges `main` and themes that screen in the same work. Recorded so it is not mistaken for an oversight.
- A user-defined or custom palette. Two themes, no theme editor.
- Per-component or per-pane theming. Appearance is one global choice.
- Any change to what the agents themselves emit, or to how the daemon reports colour. Nothing here crosses the wire.

## Technical Approach

### The palette: what is actually there, and why this is the bulk of the work

`desktop/src/styles.css:1-29` defines **19 colour tokens** — `--canvas`, `--paper`, `--paper-strong`, `--ink`, `--muted`, `--faint`, `--line`, `--line-strong`, four accent families (`--teal`, `--green`, `--amber`, `--red`, `--violet`) each with a `-soft` variant, and `--terminal` — plus three layout tokens.

Measured on this branch: **150 hex literals sit outside that block, across ~105 distinct values**, plus **27 more in `.ts`/`.tsx`**. `:root` itself duplicates two of them as bare `color:` and `background:` declarations (`styles.css:3-4`).

So the honest framing is not "tokenise a few stragglers". Roughly six sevenths of the colour in this app is a literal, and every literal stays light in dark mode. **The tokenising pass is the largest single piece of this PRD, and skipping any of it does not save work — it ships a half-dark app.**

The values are not arbitrary. They cluster into families that are already named: `#5e9c90`, `#3a6a5f`, `#72c3b3`, `#278d80`, `#0a6259` are all teal at different depths, and green, amber and red have the same shape. The work is a **semantic pass**, not a find-and-replace: decide which literals are the same *role* at a different depth, collapse them onto a named palette, and give each role a dark value.

Both directions of error are real and pull opposite ways. Collapse too eagerly and deliberate depth distinctions flatten — the app loses its layering, which is the thing that makes a dense control surface readable. Collapse too little and the result is ~105 tokens, which is not a palette; it is the same problem with longer names. **Target: roughly 25–40 tokens.** That is an expectation to check the result against, not a quota to hit.

Where a literal is genuinely theme-invariant it stays a literal, with a comment saying why and the guard's opt-out marker. The terminal surface is the main such case.

### The terminals stay dark, and that is the decision

The panes are xterm.js fed raw PTY bytes. We control the xterm background, foreground and the 16 ANSI slots; we do **not** control what an agent emits into them. Agent CLIs choose colours for their user's terminal, and two of those choices break on a light background: dim greys tuned to read on black become near-invisible, and anything emitting truecolor SGR (`\e[38;2;r;g;b m`) bypasses the 16-slot palette entirely, so remapping cannot rescue it.

The failure mode therefore lands on the one surface people actually work in, and it is not a failure we can prevent from here. So the terminals keep `--terminal: #141817` in both themes. The app chrome around them themes normally; the pane stays a dark inset, which is what it already looks like today in light mode and is a common, defensible treatment in developer tools.

**A "terminal follows the app appearance" setting was considered and deliberately deferred.** It would be cheap to add — one more enum in a section that exists — but shipping it means shipping a control whose "follow" position is unreadable against some agents, and the correct default cannot be chosen without measuring what Claude Code, opencode and codex actually emit onto a light background. That measurement is worth doing; it is not worth blocking this on. If it is done later and the answer is favourable, the setting is a small addition to an Appearance section that will already exist.

### Delivering the two themes

Light values stay on bare `:root`, so a document with no media query support and no override still renders the app exactly as it does today. Dark values are declared twice, and both are required:

- `@media (prefers-color-scheme: dark)` under a `:root:not([data-theme="light"])` guard, so the OS default applies unless the user has explicitly chosen light;
- `:root[data-theme="dark"]`, so an explicit dark choice wins on a machine whose OS is light.

The override attribute is set on `document.documentElement` from the persisted setting; `System` removes the attribute entirely and lets the media query decide. `prefers-color-scheme` works inside the Tauri webview, so the default path needs no Rust involvement — only the override needs storage, which is #803's store.

`<meta name="color-scheme" content="light">` becomes `light dark`, which is what makes the webview's own scrollbars, form controls and default backgrounds follow rather than staying light under a dark page. `<meta name="theme-color" content="#f3f0e9">` needs a dark counterpart via `media` attributes.

### Where the override is stored

In #803's settings document — `platform::paths::config_dir()/desktop.toml`, an `[appearance]` section holding one enum. **Not** in `localStorage`, and specifically not through `modeScopedKey`: that helper scopes persisted keys by runtime mode (`.fixture` / `.live`) so a fixture visit cannot poison live state, which is right for project drafts and wrong here. A theme choice is genuinely global, and a user who previews the fixture should not find their appearance reset in the live app.

### Cross-version safety

**CLAUDE.md rule 12 — did this change the TUI↔daemon contract? No.** `PROTOCOL_VERSION` stays **8** (`src/daemon_protocol.rs:227`). This is CSS, one webview attribute, and a field in a client-owned settings document. The daemon cannot observe any of it, no verb is added, and nothing crosses the wire. No `.breaking.md`, no cross-version manual test. Patch bump.

### Testing: what rule 4 means here

The mapping is stated rather than assumed, because rule 4's L1/L2 vocabulary is the Rust TUI's and this lands in a Tauri webview.

- **Frontend (vitest + jsdom, `desktop/vite.config.ts:20-24`)**: the override sets and clears `data-theme` on the document root for each of the three choices; the choice round-trips through the settings store; `System` removes the attribute rather than writing a value; the Appearance panel renders the current choice and updates on change. Note the CI job that runs these, `desktop-web`, is **advisory** — it is not one of the four required checks.
- **Rust (`cargo test-fast`, and the `build` job is a required check)**: the `[appearance]` section's serde round-trip, its default, and its tolerance of an unknown value — covered as part of #803's store tests.
- **The guard is the load-bearing test.** A lint over `desktop/src` failing on a hex literal outside the token block is what keeps this correct after the PR merges. Without it the dark theme decays one component at a time, invisibly, because nothing renders dark mode in CI.
- **What is not covered, plainly**: no automated check asserts that the app *looks* right in either theme. There is no visual regression tooling and no driver-level tier in this repo at all — `playwright`, `tauri-driver`, `webdriver` and `selenium` return zero matches workspace-wide. The compensating control is the manual smoke step added to `docs/develop/desktop-gui.md`: open the app in both OS appearances, walk every sheet and dialog, and confirm the override wins in both directions.

**The tokenising commit has a property worth stating separately, because it is the only part a reviewer can verify cheaply: light mode must be visually unchanged.** A diff of `- #5e9c90` / `+ var(--teal-deep)` repeated 150 times is not reviewable as text. Checking out that one commit and seeing an app identical to `main` is.

## Success Criteria

- On a machine set to dark, the app opens dark, with no configuration and no restart.
- The override wins in both directions: dark app on a light OS, light app on a dark OS, and `System` follows the OS live.
- The choice survives an app restart, and is not affected by visiting the fixture preview.
- Every surface the app has today is themed — deck, all four config sheets, the settings sheet, the command palette, the shortcut dialog, the confirmation dialog, the connection banner and every failure state. No component is left light-on-dark or dark-on-light.
- Light mode after the tokenising commit is visually indistinguishable from `main`.
- The webview's own chrome — scrollbars, form controls, default background — follows the theme rather than staying light.
- A new hex literal added to `desktop/src` outside the token block fails the build.
- The terminals are unchanged in both themes, and the app around them reads correctly with a dark pane inset in light mode.

## Milestones

- [x] **M1 — The semantic palette, light only.** Every hex literal in `desktop/src` collapsed onto a named token; `:root`'s bare `color`/`background` moved onto tokens; deliberate exceptions marked. **Property: light mode is visually unchanged.** Landed as its own commit so that property stays checkable. — `e9e1a24`
- [x] **M2 — The hex guard.** A lint failing on a hex literal outside the token block, with a per-line opt-out comment for deliberate exceptions, in the `xtask/linkage-check` idiom. — `8e75912`
- [x] **M3 — The dark palette.** A dark value for every token; the `prefers-color-scheme` block under its `:root:not([data-theme="light"])` guard; the `:root[data-theme="dark"]` override block; `color-scheme` and `theme-color` meta corrected.
- [x] **M4 — The override, end to end.** The Appearance section in #803's settings surface; Light / Dark / System persisted to `[appearance]` in the settings document; applied immediately; `System` clears the attribute. Vitest coverage for each.
- [x] **M5 — Docs and changelog.** `docs/develop/desktop-gui.md` gains the theming conventions, the guard's opt-out, and the both-appearances manual smoke step. Changelog fragment.
- [ ] **M6 — PRD #745's overview screen** — deferred until #779 merges to `main`, then merge and theme it. Not part of the initial PR.

## Risks

- **The tokenising pass is unreviewable as a diff, and a mistake in it is invisible until someone opens the app.** 105 colour decisions cannot be checked by reading text. Mitigated by isolating it in one commit with a stated visual-invariance property, and by the fact that a wrong token is a cosmetic bug rather than a correctness one.
- **Over-collapsing flattens the design.** The app's density depends on layered surfaces, and mapping four near-identical greys onto one token would be a real regression that no test catches. The 25–40 target exists to make the outcome checkable; the reviewer's question is "did anything lose its depth", not "is the number right".
- **Hex drift after the merge.** Every new component is an opportunity to hard-code a colour, and nothing renders dark mode in CI, so the theme decays silently. M2 is the whole mitigation and is not optional.
- **The guard will fail on PR #779's overview block**, which hard-codes light hexes and adds no new custom properties. Whichever of the two lands second inherits the cleanup. The per-line opt-out means it is never a hard block, and M6 is where this branch pays its share.
- **A dark theme nobody has looked at in every state.** Failure states, empty states and the incompatible-daemon banner are exactly the surfaces least likely to be opened during development and most likely to be wrong. The manual smoke step enumerates them for that reason.

## Open Questions

All decided, on 2026-09-02, with the user offline and having asked for judgement calls to be made and recorded rather than deferred. Each is cheap to reverse.

1. **Do the terminals follow the app appearance?** **No** — they stay dark in both themes. Reasoning in Technical Approach: the readability failure lands on the working surface and is caused by agent output we do not control. The user raised this directly and chose to leave the terminals as they are.
2. **Is a "terminal appearance" setting shipped anyway, defaulting to dark?** **No, deferred.** It is cheap to add later to a section that will exist, and shipping it now means shipping a control whose other position we have not measured.
3. **How many tokens?** Target 25–40, judged on whether the app keeps its layering rather than on the count.
4. **Does this ship behind the `experimental` flag (rule 9)?** **No.** The flag does not reach the Tauri app by any route — see #803's Open Question 1 and `prds/176-desktop-gui.md` decision 6. The user confirmed this on 2026-09-02.

## Work Log

### 2026-09-02 — M3, M4 and M5 landed; dark measures better than light

`c72fc0a` (dark palette), `ba451b3` (sheet and override), `2223c52` (registry collapse, `--scrim-rgb` rename), `9afc895` (docs and changelog).

**Contrast was measured through a real engine rather than estimated.** The stylesheet was resolved in chrome-headless-shell over CDP, reading `getComputedStyle` for every token in four scenarios, and WCAG ratios computed over **248 pairs** — 124 per theme, the identical pair list for both, because "does dark meet AA" is only meaningful next to "does light". **Dark has 22 pairs below their bar; light has 34.** No dark body text falls below AA. The delivery was verified in the same run: OS-dark with no attribute resolves identically to `data-theme="dark"`, OS-dark with `data-theme="light"` resolves identically to plain light — the `:not([data-theme="light"])` guard working in the direction easiest to get wrong — and no token failed to resolve.

**Light mode turned out to be the worse theme on accessibility, and that is a pre-existing defect rather than a regression.** `--faint` sits at 2.65–3.05:1 on every surface it renders on and `--muted` at 4.38–4.46:1 on two surfaces. Since light values were frozen for M1's invariance property, these were left alone and the dark counterparts were set to clear AA rather than mirroring the deficiency — [#821](https://github.com/vfarcic/dot-agent-deck/issues/821). One dark item sits exactly at 4.50:1 (`--status-error` on `--shell`, where light scores 4.08) — [#822](https://github.com/vfarcic/dot-agent-deck/issues/822). The remaining dark items below bar are decorative hairlines that WCAG 1.4.11 does not bind, and nearly all improved on their light counterparts; the focus ring, which 1.4.11 *does* bind, went from 3.44–3.97 to 6.94–9.18.

**A fourth token that must not invert, which this PRD did not anticipate.** The plan named `--on-accent`, `--dialog-edge` and the instrument chrome. It missed `--ink-rgb`, whose **name lies about its role**: all nine call sites are scrims and drop shadows, so tracking `--ink` into dark would cast white shadows and a white scrim and *light the app up*. It is `0 0 0` in dark, and has since been renamed `--scrim-rgb`. This is a third category the palette's framing did not have — not "changes in dark" or "stays the same", but **"tokens whose name disagrees with their role"** — and it is the one most likely to be got wrong by anyone applying the mapping mechanically. Worth carrying into any future palette work.

Three instrument-chrome tokens did need a nudge, from one cause: with a dark canvas, `--shell` at its light value sits *above* `--paper-strong`, so the tab strip and agent footer would read as floating **on top of** the tile rather than as an inset **inside** it. Pulling the family down ~7 units restores the relationship, and incidentally fixed two pre-existing sub-AA cases. The one relationship that could not survive is the light theme's 14.5:1 step between the instrument chrome and the page — unrecoverable once the whole app is dark without making the tiles near-white — so the step was kept perceptible (1.16, against the light theme's own terminal-to-tab-strip step of 1.22) and every such boundary confirmed to carry a 1px border.

**A second guard was added.** `check_dark_palette` asserts the two dark blocks declare the same tokens with the same values, that every light colour token has a dark counterpart, and that no token is dark-only. Nothing else in the repo can see that class of drift: the colour guard only asks whether a colour came from a token, nothing renders dark mode in CI, and the two blocks are necessarily separate rules so divergence is invisible in review.

**The app was opened this time**, which M1's entry could not manage. `tauri dev` still cannot run here, but the built frontend does not need Tauri — `selectRuntimeMode` falls back to fixture mode without `window.__TAURI_INTERNALS__` — so the real bundle was served and driven in headless Chrome: the deck in light, OS-dark and both override directions, plus dark captures of the command palette, the config sheets and the shortcut dialog, plus a live click-Dark-and-observe-`data-theme` run, with zero console errors. **This is Chromium, and the app ships on WebKitGTK and WKWebView** — `color-scheme: light dark` and `theme-color` with `media` attributes are the two things most likely to differ, and neither is verifiable from here. That is what M5's manual smoke subsection exists for, and it says so.

### 2026-09-02 — M1 and M2 landed; the token count came in at 66, not 25–40

`e9e1a24` (tokenising) and `8e75912` (guard). The measured before/after: **150 hex literals outside the token block became 0**, 12 `rgba()` literals became 0, two bare `white` keywords became 0, and the 21 literals in `.ts`/`.tsx` — all of them the xterm palette — stayed, each carrying an opt-out marker. All 22 pre-existing `:root` declarations survive byte for byte; nothing was renamed or revalued. The distinct-value count outside the block was **107**, not the ~105 this PRD estimated.

**The 25–40 target was missed deliberately and the reasoning is accepted.** The source holds 126 distinct values, so 25–40 tokens means folding ~3.5 values into each, which at this palette's spread implies per-channel moves of 40–60/255 — visible ones. Since this PRD states that light-mode invariance outranks the token count, and that over-collapsing is the error nobody notices until it ships, the implementation set an explicit policy instead: **fold two literals onto one token only when they differ by ≤ 24/255 per channel (~9%), and they are not layered directly against each other, and they are not a rest/hover pair.** That policy produced 66, and four of those tokens exist solely because the policy refused a fold.

The count is also less alarming than it reads. **Seventeen of the 66 are the instrument chrome** — the dark surfaces wrapped around the terminals (rail, tab strip, agent footer, output panels, reader overlay) — which under this PRD's terminals-stay-dark decision mostly keep their light-mode values in dark mode. Three more are channel-form aliases (`--ink-rgb` and friends, the first of which was later renamed `--scrim-rgb` — see the M3/M4 entry) that exist so one token serves eleven alpha depths. So the palette that genuinely needs a second, dark value is closer to 46 than to 66.

**The threshold is the lever if 66 is judged too many**, and raising it to ~40/255 would merge roughly a dozen more, mostly in the chrome. That is cheap to do later and expensive to undo, which is the right way round.

**Visual invariance was verified mechanically, not by looking at the app, and that gap is real.** No display is available in this worktree and `tauri dev` needs GTK/WebKit deps this box lacks (issue #771), so **nobody has seen this build**. What was done instead: both stylesheets were resolved through a real headless browser engine — every rule applied to a probe element under its own `:root`, then `getComputedStyle` read back for 17 colour properties, with alpha composited over the canvas — giving **399 declarations compared, zero structural differences, worst effective per-channel change 23/255**, on the connection lamp's 1px halo. That is the browser doing `var()` resolution and compositing rather than a human reading a diff, and it independently proves every introduced `var()` actually resolves. A textual round-trip expanding every `var()` back to its literal agreed.

What it cannot cover is whether a collapse *reads* wrong rather than measures wrong — a surface that stops separating from its parent, or a state change that stops signalling. **That is the review's job tomorrow**, and the places to look, in order: the connection lamp's connected fill and halo (Δ23, the largest single move, safe only because a 2px border separates them), the reader overlay's head/panel losing a 6-unit lift, the reader-open button's hover no longer changing its background, and the rail's idle nav icons.

One correction, to the implementation brief rather than to this PRD: it listed `AgentTile.tsx` as PRD #745's and therefore out of bounds. It is not — it arrived in `daf94f0` (PR #416) and is on `main` today, so it was correctly treated as in scope. Only `AgentOverview.tsx` is #745's, and it is confirmed absent from this branch, which is the check that mattered.

### 2026-09-02 — Created, merged into #803's PR

Written after the user chose to combine this PRD with #803 and ship both in a single pull request, on the reasoning that #803's settings page needs a first tenant and this PRD needs somewhere to put an override.

Two decisions were settled in that conversation and are recorded above: the terminals stay dark and unchanged, which removes the xterm 16-slot remapping, the truecolor problem and a proposed measurement task from this PRD's scope entirely; and the hex-literal cleanup is in scope, having been re-measured at **150 occurrences across ~105 distinct values** rather than the four the initial reconnaissance had quoted as examples. That re-measurement is what moved the tokenising pass from a tidy-up to the largest single piece of the work, and is why M1 is isolated with a visual-invariance property.

PRD #745's overview screen is deliberately excluded until #779 merges to `main`.
