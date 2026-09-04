# PRD #744: Zoom and text-size controls on the platform's standard keybindings

**Status**: Draft — written from the placeholder, audited once (see the Work Log), awaiting review before implementation.
**Priority**: Medium
**Created**: 2026-09-04
**Issue**: [#744](https://github.com/vfarcic/dot-agent-deck/issues/744)

## Problem Statement

The desktop app has no zoom. Every **text** size in it is a fixed pixel value chosen once, and there is no way for a user to change any of them.

That is worse than it sounds, because of how small those values are. `desktop/src/styles.css` carries **164 `font-size` declarations and every one of them is in `px`**, ranging from 6px to 20px: four declarations are 6px (`styles.css:628,676,677,754`), two are 6.5px, the agent footer is 6.8px (`:539`), the tab strip is 8px, and 21 declarations are 7px. On a high-DPI display at the default 1440×920 window those are legible; on a 4K panel at native scaling, or for anyone whose eyes are not the author's, several of them are not. The terminals are separate and larger — xterm's own `fontSize` option is 13.5px (`TerminalViewport.tsx:57`) — and equally unchangeable. There is no menu item, no setting, and no keybinding.

A Tauri window gets none of this for free. It is not a browser tab: no zoom shortcuts, no View menu, no zoom persistence. The webview's own hotkey polyfill exists but is switched off by default and is the wrong tool here — see [Why not just flip `zoomHotkeysEnabled`](#why-not-just-flip-zoomhotkeysenabled).

**Correcting the placeholder on one point.** The issue says *"the only keyboard shortcut wired in the app is `Cmd/Ctrl+K` for the command bar (`desktop/src/App.tsx:167`)"*. The line has moved to `App.tsx:232`, and the claim is narrower than it reads: `App.tsx`'s handler also binds `Escape`, `?`, `1`–`4` and `j`/`k` (`App.tsx:229-255`); `AgentComposer.tsx:108` distinguishes `Enter` from `Shift+Enter`; and `OutputReader.tsx:53-59` registers a **second** window-level `keydown` listener of its own for `Escape`. What is true is the part that matters here: ⌘K is the only `metaKey`/`ctrlKey` binding in the app, and nothing anywhere binds a zoom key. The second fact is load-bearing later — a third window-level listener is established practice in this app, not a novelty (see [Where the key handling lives](#where-the-key-handling-lives)).

The accessibility framing in the issue is the right one and is worth restating as the actual requirement: **an unreadable terminal is the whole product being unusable.** Everything below is arranged around the terminals rather than around the chrome.

## Solution Overview

One zoom level for the window, on the platform's standard keys, applied by the webview, persisted in the app's own settings document, and visible and editable in the Settings sheet.

- `Cmd +` / `Cmd -` / `Cmd 0` on macOS, `Ctrl` equivalents elsewhere, stepping a fixed ladder of ten levels from 75% to 300% with 100% as the reset.
- Applied with `webview.set_zoom()` (Tauri v2), which scales the whole page — chrome and terminal canvases alike — so there is one mechanism rather than two that can drift apart.
- Persisted in `desktop.toml` as a new `[zoom]` section on the PRD #803 settings document, and re-applied Rust-side at launch before the webview boots, so there is no flash of unzoomed UI.
- The terminals get **explicit** handling: every zoom change triggers a coalesced re-fit, so `@xterm/addon-fit` recomputes rows/cols and the daemon is told the new PTY size through the resize path that already exists and already coalesces.
- The keys are claimed in the **capture phase**, because xterm would otherwise type them into the focused agent's PTY — a measured clash, not a hypothetical one. See [Where the key handling lives](#where-the-key-handling-lives).
- A **Zoom** section in the Settings sheet showing the current level, because a webview zoom has no indicator of its own — and one row in the `?` shortcut overlay.

## Scope

### In Scope

- **The three keybindings**, bound on both `event.key` and `event.code` so the numpad and the shifted forms (`+`, `_`) work, and so a non-US layout is not silently excluded.
- **Capture-phase interception** of those keys, so they never reach xterm.
- **`webview.set_zoom()` as the single mechanism**, driven by a new `desktop_set_zoom` Tauri command alongside the existing `desktop_*` commands.
- **A `[zoom]` section on `DesktopSettings`** (`desktop/src-tauri/src/settings.rs`), normalised on read the way `AppearanceMode::from_str_lossy` already normalises an unknown appearance token.
- **The launch-time apply**, Rust-side, from the same document.
- **An explicit, coalesced terminal re-fit on every zoom change**, routed into the bridge's existing per-agent coalesced resize.
- **A Zoom section in the Settings sheet** — one registry row and one panel, per the `settingsContract.ts` contract.
- **One row in the `?` shortcut overlay** (`App.tsx:621`).
- **A near-zero `App.tsx` footprint**: one import, one hook call, one array entry. Nothing else. This is a deliberate constraint, not an aesthetic one — see [Staying out of #869's way](#staying-out-of-869s-way).
- **Docs**: a Zoom section in `docs/develop/desktop-gui.md`, and the manual smoke check extended to cover what jsdom cannot reach.

### Out of Scope

- **Per-pane zoom.** Settled with the user on 2026-09-04: global only. It is not merely a cost decision — webview zoom is a property of the *webview*, so a per-pane level is unreachable through this mechanism at all and would need a second one (per-terminal `fontSize`), with per-agent persistence and per-agent resize accounting on top. Recorded as an open question so the option is not lost.
- **A CSS / root-font-size mechanism.** Rejected on measurement — see [The mechanism decision](#the-mechanism-decision).
- **`Ctrl` + scroll-wheel zoom**, which the Tauri polyfill binds. Deliberately not: the app's largest scrollable surfaces are terminal panes, and a trackpad gesture that zooms the whole window while someone is scrolling agent output is a misfire waiting to happen. If it is wanted it is a small, separable follow-up.
- **A native menu bar with a View menu.** The app has no menu today; adding one is its own piece of work and the keybindings do not depend on it.
- **Window size and position persistence.** Also client-owned presentational state, also absent, and genuinely unrelated plumbing.
- **Any daemon-side change.** There is none: no new protocol verb, no field, no bump. See [Rule 12: no contract change](#rule-12-no-contract-change).
- **The `experimental` feature flag.** Settled by precedent, not re-decided — see [Feature flag](#feature-flag).
- **Anything that makes the fixture/browser preview zoom itself.** The browser already has zoom; see [The fixture preview](#the-fixture-preview).
- **Fixing the existing `localStorage` keys.** This PRD adds none and touches none; [#824](https://github.com/vfarcic/dot-agent-deck/issues/824) owns them and its count is already out of date — see [Persistence](#persistence-and-the-boundary-question).
- **A driver-level or browser-level test tier.** None exists ([#823](https://github.com/vfarcic/dot-agent-deck/issues/823), [#836](https://github.com/vfarcic/dot-agent-deck/issues/836)) and this PRD does not build one. What that costs is stated plainly in [Testing](#testing-what-rule-4-means-here) rather than glossed.

## Technical Approach

### The mechanism decision

The issue is emphatic that webview zoom and a CSS/root-font-size change not be conflated, so this is decided here, on measurement.

**Decision: webview zoom.** `webview.set_zoom(scale)` — `webkit_web_view_set_zoom_level` on Linux, `WKWebView.setPageZoom` on macOS 11+, WebView2's zoom factor on Windows (`wry-0.55.1/src/{webkitgtk,wkwebview,webview2}/mod.rs`).

The measurement that settles it: **the stylesheet contains zero `rem` units.** `grep -o '[0-9.]\+rem' desktop/src/styles.css` returns nothing; there are 164 `font-size` declarations, all `px`, and 960 `px` literals in the file. A root font-size is therefore not a scaling seam in this app — it is a *proposal to create* one, by rewriting 960 literals across a 1073-line stylesheet, before a single pixel scales. And it would still leave the terminals untouched, because xterm renders to a canvas from its own `fontSize` option, so the CSS route needs a second mechanism for the panes and then has to keep the two in step.

Webview zoom needs no stylesheet change at all, and it scales the terminals with everything else. Two further facts came out of checking it, both of which strengthen the choice:

- **The existing responsive breakpoints do the reflow work.** Page zoom shrinks the *CSS-pixel* viewport, so at 1440 × 1.5 the app lays out at 960 CSS px and the `@media (max-width: 960px)` rules fire — the layout degrades exactly as it does when you narrow the window, which is a path already designed and already exercised. There are breakpoints at 1450, 1260, 960, 680 and 420 (`styles.css`).
- **`px` font sizes are a liability for the CSS route and neutral-to-helpful for this one.** Page zoom scales a fixed-px design uniformly and keeps its proportions, which is what page zoom is for. (Note the stylesheet is not px-*only*: it also carries 7 `vh`, 5 `vw`, 6 `ch`, 32 `em`, 55 `%` and 43 `fr` values. It is the *font sizes* that are uniformly px.)

The ladder is `0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0` — browser-familiar values, ten steps, `1.0` the reset.

**The 3.0 ceiling is set by width, and by clipping rather than by taste.** `body` is `overflow-x: hidden` (`styles.css:339`), so horizontal overflow is *clipped* rather than scrollable, while vertical overflow simply scrolls — which makes width the binding constraint and height a non-constraint. `html, body, #root` declare `min-width: 320px` (`styles.css:338`) and `tauri.conf.json` declares `minWidth: 1024`, so 1024 / 3.0 = 341 CSS px is still above the floor while 1024 / 3.2 = 320 reaches it exactly. 3.0 is the last ladder step that stays clear of the point where content starts being clipped at the smallest window the app allows.

**The 0.75 floor is set by how small the type already is.** The agent footer is 6.8px, which renders at 5.1px at 0.75 and 4.6px one browser step lower. Zooming out past the point where the chrome is unreadable is not a feature. Open Question 1 asks whether that is the right trade.

### Why not just flip `zoomHotkeysEnabled`

Tauri v2 has a window option that looks like this whole PRD in one boolean, so here is why it is not used.

On macOS and Linux — the cfg is `#[cfg(all(desktop, not(target_os = "windows")))]` at `tauri-2.11.5/src/manager/webview.rs:554` — `zoom_hotkeys_enabled` makes Tauri inject an initialization script (`src/webview/scripts/zoom-hotkey.js`, 43 lines) that binds `Ctrl/Cmd` with `-`, `=`, `+` and `0`, plus the wheel. Reading it is the argument against it:

- **It does not persist anything.** The level is a module-local `let zoomLevel = 1` (`:7`), so every launch is 100%. Persistence is half of what #744 asks for.
- **It fires one un-coalesced IPC per keystroke** — `invoke('plugin:webview|set_webview_zoom', …)` straight out of the handler (`:24-26`). That is the exact shape the issue asks us not to build.
- **It never tells the terminals anything.** No re-fit, so the daemon is never told the new rows/cols except by whatever the `ResizeObserver` happens to catch.
- **Its range is 0.2 to 10.0 (`:9-10`), stepped by an additive ±0.2** — so a step is 20 percentage points, not 20%: 1.0 → 1.2 is +20%, but 2.0 → 2.2 is +10%. At 10× this app is unusable, and at 0.2 the footer type is 1.4px.
- **Its keydown handler calls no `preventDefault()`** (`:12-28`) and does not stop propagation, so the keys also reach the page — which for this app means reaching xterm (see below). Its *wheel* handler does call `preventDefault()` (`:32`), and listens on the deprecated non-standard `mousewheel` event.
- **It is not one behaviour across platforms.** On Windows the flag is not the polyfill at all — it sets WebView2's `IsZoomControlEnabled` and, on a recent enough runtime, `IsPinchZoomEnabled` (`wry-0.55.1/src/webview2/mod.rs:572,593`), whose steps, range and gesture handling are the runtime's rather than ours.
- **It needs the same permission we would otherwise need anyway** (below), so it does not even save that.

It is, however, good evidence about the *shape* of the answer: the platform-standard binding for a webview really is `Cmd/Ctrl` with `-`, `=`/`+` and `0`, and the platform-standard implementation really is a webview zoom call. We build the same shape properly.

### The permission, and why the call goes through Rust

**Measured: `core:webview:default` does not include `allow-set-webview-zoom`.** The default set is exactly `allow-get-all-webviews`, `allow-webview-position`, `allow-webview-size`, `allow-internal-toggle-devtools` (`tauri-2.11.5/permissions/webview/autogenerated/reference.md`), and this app's only capability is `["core:default"]` (`desktop/src-tauri/capabilities/default.json`). So the frontend route — `getCurrentWebview().setZoom()` — would be denied until the capability grows `core:webview:allow-set-webview-zoom`.

The zoom therefore goes through a **`desktop_set_zoom` Tauri command**, for three reasons and not merely to dodge a capability edit:

1. **It needs no capability widening.** Application commands registered in `tauri::generate_handler!` are not gated by the core permission sets; broadening the webview's own core surface to reach a call we can make ourselves is strictly more privilege for no gain.
2. **It is consistent with every other bridge call in this app.** All nine commands in `generate_handler!` (`lib.rs:1077-1087`) begin with `ensure_main_webview` (`lib.rs:606-612`), and eight of the nine are reached from the frontend through the `DeckBridge` seam — the exception is `desktop_get_snapshot`, which no frontend code invokes today. A `setZoom` on `DeckBridge` means fixture mode no-ops structurally, exactly as `resizeTerminal` already does.
3. **Rust needs the call anyway**, for the launch-time apply below. One implementation, two callers.

### Persistence, and the boundary question

The level lives in `desktop.toml`, as a new section on `DesktopSettings`:

```toml
[zoom]
level = 1.25
```

This is the right side of the ownership boundary and did not need re-deriving: `docs/develop/desktop-gui.md`'s Client-owned row **already names zoom** — *"settings … plus genuinely presentational state: window size and position, focused tab, zoom"*. A zoom level describes this display and nothing about the work, the project or the machine the agents run on.

It is deliberately **not** another `localStorage` key. [#824](https://github.com/vfarcic/dot-agent-deck/issues/824) records those as being on the wrong side of the boundary and asks nobody to add one more by pattern-matching. **Its count is stale, and the way it went stale is the argument for taking it seriously**: #824 was filed naming four `modeScopedKey` keys, and there are now **five** — `OVERVIEW_COLUMNS_STORAGE_KEY` (`AgentOverview.tsx:439`) landed the day after, in PRD #745's column picker. The warning in #824's own body — *"the next contributor can add a fifth `localStorage` key by pattern-matching without ever meeting the boundary rule"* — has already come true once. (A sixth key, `FIXTURE_SETTINGS_KEY` at `bridge.ts:167`, is deliberately *not* mode-scoped and belongs to the settings store itself. And #869 removes `useProjects.ts` outright, so the count is about to move again in the other direction.) This PRD adds none of them: a per-installation *preference* is what `desktop.toml` is for, while those keys are per-project *content* awaiting [#819](https://github.com/vfarcic/dot-agent-deck/issues/819).

Two consequences of using the #803 store, both wanted:

- **The save path is already right.** `useDesktopSettings` applies optimistically, serialises writes one at a time and drops a superseded response, so the last level chosen is the last one on disk. The panel and the keybindings both go through it and cannot disagree.
- **`default_document_shape_is_pinned` will fail**, by design. The docs say so: *"a new field fails that test by design — updating it is the deliberate act that puts the ownership question in front of a reviewer."*

**Normalisation is Rust-side and lossy, matching precedent.** `level` is an `f64`. A hand-edited `99`, `0`, `-1`, `NaN` or `inf` is clamped to the nearest ladder value rather than rejected, exactly as `AppearanceMode::from_str_lossy` folds an unknown token to the default — losing a whole document over one bad field is the opposite of the unknown-key tolerance the schema is built for.

**Why the launch apply is Rust-side and the change apply is frontend-side.** Two writers, one situation each, no overlap. Rust reads the document in `setup()` and calls `set_zoom` before the webview has run any of our JavaScript, so there is no flash of 100% and no dependence on the frontend booting. The frontend applies only on *change*.

**This is a deliberate departure from #743's arrangement, and the argument against it should be read before the argument for it.** `App.tsx:105-111` applies the appearance on load *and* on change from one effect, and its comment says that is what buys *"no restart and no second path that could disagree with this one"* — i.e. #743 concluded that a single effect covering both situations is what produces a single path. Splitting by situation, as this PRD does, is the two-writer arrangement that comment was avoiding. It is chosen anyway because zoom has something appearance does not: a Rust-side option that removes a *visible* defect, the flash of 100% UI before the webview's first paint applies the stored level. The price is exactly the failure #743's comment predicts, it is named in [Risks](#risks), and it falls on the one leg with no automated coverage. If the flash turns out not to be perceptible in practice, collapsing to #743's single-effect shape is the better design and should be taken.

### The terminals: re-fit, not just re-scale

This is the constraint the issue flags as most likely to bite, and it is the part that makes this more than a settings row.

`TerminalViewport` builds one `FitAddon` per pane and wires it to a `ResizeObserver` on the host (`TerminalViewport.tsx:118-128`). `fit()` recomputes rows/cols from the host's CSS-px box and the terminal's font metrics, then calls `onResize(cols, rows)`, which lands in `TauriDeckBridge.resizeTerminal` (`bridge.ts:1120`).

**The existing coalescing is exactly what the issue asks us to reuse, so we reuse it unchanged.** `resizeTerminal` records the pending size per agent and calls `scheduleResize`, which takes a `requestAnimationFrame` and refuses to queue a second one while a frame or an in-flight invoke is outstanding; `flushResize` sends one `desktop_terminal_resize` and, on settling, re-schedules only if something newer arrived (`bridge.ts:1120-1153`). So a held zoom key cannot produce a resize storm at the daemon: at most one in-flight resize per agent, coalesced to the newest size. The nearest analogue on the Rust side is `SNAPSHOT_COALESCE_INTERVAL` (`desktop/src-tauri/src/lib.rs:73`), a 150ms floor on snapshot emission. **We add no new coalescing to the daemon path and change none of it.**

What we do add is the **trigger**, and it is explicit rather than implicit. Under page zoom the `ResizeObserver` should fire on its own, but only because of the pane's **width**: `.deck-main`'s width descends from the body, so it shrinks in CSS px as the viewport does. The **height** does not follow the viewport across most of the ladder — `.agent-panel` is `min-height: 320px; height: 42vh; max-height: 560px` (`styles.css:475`), and at the declared default window height of 920 the `42vh` term is 386px at 1.0× but 309px at 1.25×, so from 1.25 upward it is pinned at the 320px floor and stops changing. That is **six of the ten ladder steps** with a frozen pane height. The observer still fires, on width; but "the box shrinks with the viewport" is only half true, and it is exactly the kind of implicit dependency that a future pane sized in fixed px would break silently, while jsdom — which runs no layout — can never observe it either way. So:

1. `terminalRegistry` grows a **re-fit seam** — `TerminalViewport` registers its `fit` alongside the terminal instance it already registers, and a `refitAllTerminals()` walks them.
2. `refitAllTerminals()` is itself **rAF-coalesced**, one pass per frame no matter how many callers ask. This is the part that protects the *client*: `fit()` measures layout, so running it per keystroke across every mounted pane is one forced reflow per pane per keystroke.
3. The zoom hook requests a re-fit after the `set_zoom` call resolves. The observer remains as the backstop that catches the real post-layout geometry, since neither WebKitGTK nor WKWebView promises the zoom has been laid out by the time the IPC reply lands. Both routes end in the same idempotent `fit()` and the same coalesced daemon resize, so belt and braces costs nothing.

### Where the key handling lives

A **third** window-level `keydown` listener, in its own hook (`desktop/src/hooks/useZoom.ts`), registered **in the capture phase**. Not a branch in `App.tsx`'s handler.

**The capture phase is a correctness requirement, not a preference, and it is the finding that most shaped this section.** xterm binds its own `keydown` on `this.textarea` — the `.xterm-helper-textarea`, which is focused whenever a pane has focus — and its key evaluation contains two branches that consume exactly the keys this PRD wants (verified in `@xterm/xterm/lib/xterm.js`):

- `ctrlKey && !altKey && !metaKey && keyCode >= 48 && key.length === 1` sets the emitted key to the literal character. `-` is keyCode 189, `=` is 187, `0` is 48, so **`Ctrl -`, `Ctrl =` and `Ctrl 0` would each type a character into the focused agent's PTY.**
- `"_" === e.key && (key = C0.US)` maps **`Ctrl _` to 0x1F**, which is undo in readline and emacs.

A window-level listener in the *bubble* phase runs after the target's own handlers, so it would zoom **and** let xterm write to the PTY, and `preventDefault()` at that point cannot retract a byte already sent. A capture-phase listener on `window` runs before any listener on a descendant, so `preventDefault()` plus `stopPropagation()` there claims the keys outright and xterm never sees them. Note the platform asymmetry this resolves: the macOS binding uses `metaKey`, which that first branch excludes, so macOS would have escaped the bug and Linux and Windows would not — the kind of split that ships. `attachCustomKeyEventHandler` is the alternative and is not used: it would need wiring per terminal instance, and it puts the zoom vocabulary inside `TerminalViewport`, which has no business knowing about it.

Three further reasons the listener is its own hook rather than a branch in `App.tsx`:

- **It must fire while a terminal has focus, which is most of the time.** `App.tsx`'s handler computes `editing` — true for `.xterm-helper-textarea` — and returns early on it at `App.tsx:241`. Zoom belongs *above* that line, and putting it there means editing the middle of the one function in this app most likely to be in conflict.
- **It keeps the `App.tsx` footprint to three lines** (see below).
- **It is established practice.** `OutputReader.tsx:53-59` already registers its own window-level `keydown`, so this is the second precedent, not the first exception.

**Key matching is on both `key` and `code`.** `event.key` covers `-`, `_`, `=`, `+` and `0`; `event.code` covers `NumpadAdd`, `NumpadSubtract` and `Numpad0`, which produce `key` values that vary by platform and layout. Both are matched.

**On the clash with `App.tsx`'s own bindings there is none to resolve**, which is a narrower claim than "no clash exists": `App.tsx` binds ⌘K, `Escape`, `?`, `1`–`4` and `j`/`k`, and none of them is `-`, `=`, `+`, `_` or `0`. The clash that does exist is xterm's, above, and the capture phase is what settles it.

### On issue #826

[#826](https://github.com/vfarcic/dot-agent-deck/issues/826) is about `App.tsx:230-231` computing `editing` from `event.target as HTMLElement` on every keydown, ahead of every branch, so a keydown whose target is not an element throws before `Escape` is reached.

**The new hook neither fixes it nor inherits it**: the hook never reads `event.target` at all, so it is robust to a non-element target by construction. **But a test that mounts `ControlDeck` and dispatches on `window` does hit it**, because `App.tsx`'s listener is then also mounted and `window` has no `.matches` — which is precisely the test #826 asks somebody to add. So M3's keyboard tests drive the hook in isolation rather than through a mounted deck, and say so in a comment naming #826. Fixing #826 is deliberately not taken on here; the PR will state that the overlap was found, understood and left alone.

### `App.tsx`'s footprint

Three lines, and this is a scope commitment rather than a style note:

```tsx
import { useZoom } from "./hooks/useZoom";          // 1
const zoom = useZoom(runtime, settings);            // 2
// …and one entry in the `shortcuts` array at :621  // 3
```

Everything else — the ladder, the key matching, the capture-phase listener, the coalescing, the persistence, the panel — lives in new files.

### Staying out of #869's way

[PR #869](https://github.com/vfarcic/dot-agent-deck/pull/869) is open, +14491/-447 across 43 files, and touches `App.tsx`, `App.test.tsx`, `useDeckRuntime.ts`, `lib/bridge.ts` and `types.ts` — **all five** of the files this PRD also needs. It is PRD #819's implementation and will land as one very large change.

The response is to stay narrow, not to coordinate. Of those five: `App.tsx` gets three lines; `App.test.tsx` gains tests and changes nothing existing; and `bridge.ts`, `types.ts` and `useDeckRuntime.ts` get additive-only edits — one interface method plus two implementations and one DTO field, one field, and one pass-through respectively. No reordering, no refactoring, no moving of anything #869 might also be moving. `origin/main` gets merged when #869 lands rather than at push time.

### The fixture preview

`selectRuntimeMode()` returns `fixture` for a plain browser visit, and `FixtureDeckBridge` structurally cannot reach Tauri. A browser already has zoom, and `Ctrl -` in a browser is not reliably preventable.

So the keybindings **bind only where the bridge can actually zoom**, and the Settings row renders its control disabled in fixture mode with one short line pointing at the browser's own zoom. The alternative — binding everywhere — gives the browser preview a level that persists, appears in the UI and does nothing, while the browser's own zoom fires on the same keystroke: two zooms, one of them invisible and wrong. A setting that reads as live and is inert is worse than one that says it is unavailable.

That one line of panel prose is the exception `docs/develop/desktop-gui.md`'s text rule allows — *"an error, or a consequence the user has to act on, is a different thing and stays"* — because it tells the reader what to do instead. It appears only in the browser preview.

### The Settings section

A new registry row and a new panel, per `settingsContract.ts`: `{ id: "zoom", label: "Zoom", icon: ZoomIn, component: ZoomPanel }` in `SETTINGS_SECTIONS`. One row, `Zoom` in the 132px label column, and a `<select>` of the ten ladder levels as percentages beside it. A select rather than a segmented control because the docs' cardinality rule says so — segmented is for up to about four options, and there are ten. There is no separate reset control: choosing 100% *is* the reset.

**Two deliberate consequences, both anticipated by #803's design rather than surprises.**

First, **the settings sheet's section column comes back.** It drops the column while the registry holds one row and renders that panel full width; a second row restores it. `settingsRegistry.ts`'s own docstring says this arrives *"with no layout work on the side of whoever adds it"*, `docs/develop/desktop-gui.md:289` says the same in the second person, and both directions are already pinned with stub sections in `SettingsSheet.test.tsx`. So the Appearance panel changes width. That is the designed behaviour, and this PRD is the first real second tenant to exercise it.

Second, **this is the moment the docs names for deciding whether section headings earn their row**, and it is decided here: **no panel headings.** The docs frames it as open, expecting #741 or #802 to trigger it, and gives the argument that survives — the section list beside the panel already names the active section, so a heading repeating that name inside the panel is the same redundancy one level up. Both panels stay heading-free, which is also what `App.test.tsx:506-508` currently pins.

Zoom is its own section rather than a second row in Appearance because the contract makes a section a *feature's*. `AppearancePanel.tsx` is not exclusively #743's — its own docstring opens *"The Appearance section (PRD #743 M4), and the settings surface's first real tenant (PRD #803 M4)"*, so it is jointly owned by the feature and the container. Neither owner is #744, and a zoom row in it would make it three.

### Feature flag

CLAUDE.md rule 9 asks whether a new user-visible surface ships behind `experimental`. **No, and this is settled by precedent rather than re-decided**: PRD #176 decision 6 (`prds/176-desktop-gui.md:101`) records it for the entire desktop binary — *"a separate GUI binary has no such seam — the act of building/running it is the opt-in. So maturity is handled by packaging."* PRD #745 followed the same precedent for the same reason. Nothing about a keybinding changes it: the flag gates render/input seams inside the **TUI** binary, and there is no TUI seam here.

### Rule 12: no contract change

CLAUDE.md rule 12 applies when a change touches the daemon, the TUI↔daemon protocol, orchestration or hooks. **This one touches none of them, so no `PROTOCOL_VERSION` bump, no `.breaking.md` fragment and no cross-version manual run.** Stated precisely, because the change does cause more traffic on a daemon-facing path:

- The settings document is client-owned and the daemon cannot observe it (`settings.rs`'s module docs: *"nothing in this module crosses the TUI↔daemon protocol"*).
- The only daemon-facing effect is more `desktop_terminal_resize` calls on an existing verb, with unchanged shape and unchanged meaning — a resize caused by a zoom is indistinguishable from one caused by dragging the window edge, which is the same code path today.
- No file under `src/` is touched, and nothing is added to `src/daemon_protocol.rs`. The PR will show the `git diff --stat` for those paths as evidence rather than asserting it.

### Testing: what rule 4 means here

Rule 4 is written in the TUI's vocabulary — L1 `insta`/`TestBackend`, L2 PTY + vt100 + a `.cast`. This lands in the Tauri app, so the mapping is stated rather than assumed, and the honest summary is that **the visual half of a zoom feature is the half this repo cannot test.**

- **The L1 equivalent is vitest + jsdom + Testing Library**, measured at 11 files and 224 tests on this branch, run by the advisory `desktop-web` CI job together with `pnpm build` (which is `tsc && vite build`, so it is the type gate too).
- **The Rust half runs in the required `build` job**, because `desktop/src-tauri` is a workspace member and `cargo test-fast` is `--workspace`.
- **There is no driver-level tier and no browser-level tier.** [#823](https://github.com/vfarcic/dot-agent-deck/issues/823) records that nothing drives the real window; [#836](https://github.com/vfarcic/dot-agent-deck/issues/836) records that there is no browser-level e2e either, and that *"jsdom is a DOM implementation, not a browser — it runs no layout engine, paints nothing, and computes no geometry."*

**What that means for this feature specifically, since it is unusually exposed.** jsdom can prove the ladder, the key matching, the capture-phase interception, the coalescing, the persistence round-trip and that a re-fit was requested. It cannot prove that anything got bigger. Concretely, the following are **not covered by any automated test in this repo** and are verified only by the manual smoke check:

| Claim | Why no test here can reach it |
| --- | --- |
| The webview actually scales at each level | needs a real webview; jsdom has none |
| `fit()` yields the correct rows/cols after a zoom | jsdom measures nothing, so `fit()` has nothing to measure |
| The layout holds at 300% and at 75% | no layout engine |
| The terminal canvas re-renders crisply at the new DPR | nothing paints |
| The Rust launch-time apply happens | needs a window; no test constructs a Tauri window |

The compensating control is `docs/develop/desktop-gui.md`'s manual smoke check, extended by M6 with a zoom pass: step to 300% and back to 100% with a live agent in a pane, confirm the pane's content reflows rather than clipping, confirm the daemon was told (the agent's TUI redraws at the new size), confirm nothing was typed into the PTY by the zoom keys, and confirm the level survives a restart. Manual checks decay, and that is a real cost of shipping this, not a formality.

## Success Criteria

- `Cmd/Ctrl` with `+`, `-` and `0` change the app's zoom, and they work **while a terminal pane has focus** — the state the app is in most of the time.
- Those keystrokes reach the zoom and **not** the agent's PTY, on every platform rather than only on macOS.
- The level survives a restart, and the app comes up already at it with no visible jump from 100%.
- A zoom change re-fits every mounted terminal and the daemon learns the new rows/cols, so the agent's own TUI redraws at the new size rather than staying at the old grid.
- A held zoom key produces **at most one in-flight resize per agent** and no more than one `fit()` pass per frame — proven by test at the coalescing seams, not by inspection.
- The current level is visible and changeable in the Settings sheet, and the keybindings and the panel cannot disagree about it.
- The `?` overlay lists the zoom keys.
- Nothing daemon-side changes: no new verb, no field, no `PROTOCOL_VERSION` bump.
- No new `localStorage` key appears.
- `App.tsx` grows three lines.

## Milestones

- [ ] **M1 — The zoom model, pure and tested.** `desktop/src/lib/zoom.ts`: the ladder, `stepZoom(level, "in" | "out")`, `clampZoom`, `formatZoom`, and `zoomIntentFromKey(event)` returning `"in" | "out" | "reset" | undefined`. No DOM, no bridge, no React — so every edge case (both ends of the ladder, the shifted and numpad forms, a bare `-` with no modifier, a level off the ladder, a non-element target) is a cheap unit test. Nothing is wired up in this milestone.
- [ ] **M2 — Storage and the launch apply.** A `[zoom]` section on `DesktopSettings` with lossy clamping on read; `desktop_set_zoom` registered in `generate_handler!` behind `ensure_main_webview`; a `setup()` hook that applies the stored level to the main webview at launch. `default_document_shape_is_pinned` updated deliberately. Rust tests for the clamping (out-of-range, zero, negative, `NaN`, `inf`, absent, and an unknown sibling section surviving the merge). The apply itself has no automated coverage and M6's smoke check is what confirms it.
- [ ] **M3 — The apply path and the keybindings.** `setZoom` on `DeckBridge` plus both implementations (`TauriDeckBridge` invoking the command, `FixtureDeckBridge` no-op), `zoom` on `DesktopSettingsDto` with `normalizeDesktopSettings` handling it, one pass-through in `useDeckRuntime` and one field on `DeckRuntimeState`; `useZoom` registering a **capture-phase** listener, applying, and persisting through `useDesktopSettings` behind a coalescer; the three lines in `App.tsx` including the `?` overlay row. Tests, driving the hook in isolation rather than a mounted deck (see [On issue #826](#on-issue-826)): a keydown steps the level and calls the bridge; both ends of the ladder hold; reset returns to 1.0; **a listener on a descendant element does not see the zoom keys**, which is the xterm-clash guarantee; the keys are inert in fixture mode; N rapid keystrokes produce one final persisted value and fewer writes than keystrokes (fake timers).
- [ ] **M4 — The terminal re-fit.** A re-fit seam on `terminalRegistry` with `TerminalViewport` registering its `fit`; `refitAllTerminals()` rAF-coalesced; the zoom hook requesting it after the apply resolves. Tests: every registered pane is re-fitted exactly once per frame however many requests arrive; an unregistered pane is not called; a pane that unmounts mid-flight is not called; the existing bridge resize coalescing is asserted to be the path the refit lands in, and is not modified.
- [ ] **M5 — The Settings row.** `ZoomPanel` implementing `SettingsPanelProps`, one row in `SETTINGS_SECTIONS`, disabled with its one actionable line in fixture mode. Tests: the row shows the stored level; changing it saves the **whole** document with the appearance section untouched; the sheet's section column returns now that the registry holds two rows; neither panel gains a heading.
- [ ] **M6 — Docs, changelog and the smoke check.** A Zoom section in `docs/develop/desktop-gui.md` covering the mechanism, the ladder and both ends' justification, the capture-phase requirement and why it exists, where the level is stored, and why `zoomHotkeysEnabled` is not used; the manual smoke check extended with the zoom pass; the table of what jsdom cannot prove carried into the docs rather than left only here. Changelog fragment via the `dot-ai-changelog-fragment` skill.

## Risks

- **The re-fit is the part that can silently not happen, and its failure looks like an agent bug.** If the daemon is never told the new rows/cols, the PTY keeps the old grid and the agent's own TUI paints at the wrong width — garbled output in the pane, which reads as the agent misbehaving rather than as a zoom defect. Made worse by the `ResizeObserver` firing anyway on width: the implicit path can mask a missing explicit trigger during development and then not fire in some future layout. This is why M4's trigger is explicit and tested at its seam even though the observer exists.
- **The capture phase is a one-line property with a silent, platform-split failure mode.** Drop the `true` third argument, or the `stopPropagation()`, and the zoom keys start typing into the focused agent's PTY on Linux and Windows while continuing to look correct on macOS, because `metaKey` misses xterm's branch. M3's descendant-listener test is what pins it, and it is the single most load-bearing test in this PRD.
- **Nothing automated proves the feature does the one thing it is for.** Five claims in the [Testing](#testing-what-rule-4-means-here) table have no test in this repo and rest on a manual check. That is the standing state of desktop coverage (#823, #836), not something this PRD introduces — but a zoom feature is disproportionately exposed to it, because almost all of its behaviour is geometric.
- **The chrome is fixed-px, mostly between 6px and 8px, and has not been looked at above 100% by anyone.** The breakpoints suggest it reflows well, but "suggest" is the honest word, and the failure mode is a broken layout at a level a user with poor eyesight would actually choose. M6's smoke check has to be run at both ends, not just at 125%.
- **xterm may not re-render on a DPR change**, leaving crisp-at-100% text blurry after a zoom until something else forces a repaint. The fit-driven resize should force one, but the canvas/DPR interaction is exactly what jsdom cannot see.
- **The Rust launch-time apply is the one leg with no automated coverage**, and it is the leg #743's single-effect comment warns about. If it regresses, the app comes up at 100% while the Settings row reads 150% — two writers disagreeing, which is the specific thing splitting them by situation was supposed to be worth. Only the smoke check catches it, and if the flash it exists to prevent proves imperceptible, the better move is to delete this leg and adopt #743's shape.
- **A future refactor merging the two keydown handlers would silently kill zoom.** The `if (editing) return` at `App.tsx:241` sits above everything that follows it, and `.xterm-helper-textarea` is focused whenever a pane is. Folding `useZoom`'s listener into that handler — bubble phase, `editing` gate and all — would break zoom in the app's normal state *and* re-open the PTY-typing bug, and no test that dispatches on `document.body` would notice either. The hook's separateness is load-bearing, not stylistic.
- **#869's landing.** All five of the files this PRD needs are files #869 touches. The mitigation is the three-line `App.tsx` footprint, additive-only edits elsewhere, and merging `origin/main` when #869 lands rather than at push time.
- **Persisting per keystroke would be a write storm.** A held key produces one event per OS repeat interval, each one otherwise a temp-file-plus-rename of `desktop.toml`. The coalescer in M3 is load-bearing, and its test asserts fewer writes than keystrokes with the final value correct — not just that the final value is correct.
- **Scope creep toward a View menu.** Zoom is conventionally a menu item as well as a shortcut, and this app has no menu at all. Adding one is a separate piece of work; the Settings row is the discoverability answer here.

## Open Questions

1. **Should the floor go below 0.75?** It is set by the 6.8px footer becoming 5.1px, which assumes the current type scale is right. A user on a very large display might reasonably want 0.5 to fit more agents on screen — and would get unreadable chrome, which is a different complaint arriving from the same feature. Leaning: leave it, and revisit if anyone asks, since the answer may really be "make the base type bigger" given four declarations are already 6px.
2. **Per-pane terminal text size, as the issue suggests.** Out of scope here because webview zoom cannot express it, but the underlying want — "this agent's output matters more right now" — is real. Worth noting it may be better served by a *layout* answer than a text one: PRD #313 shipped exactly that for the TUI (zoom the focused pane, the `tmux prefix+z` model) and the desktop has no equivalent. Deciding between "bigger text in one pane" and "one pane gets the window" is the actual question, and it is not this PRD's.
3. **`Ctrl` + wheel.** Deliberately excluded above, but it is what the Tauri polyfill binds and what users of other apps may expect. If it is added it needs a rule for what happens when the pointer is over a terminal, which is where most scrolling in this app happens — and the same capture-phase reasoning applies, since xterm handles wheel events too.
4. **Does the level belong to the window rather than the installation, once there is more than one window?** Today there is exactly one, so the question is inert. It becomes real alongside [#828](https://github.com/vfarcic/dot-agent-deck/issues/828)'s two-process race, and the answer may well be "per window, not persisted per window".
5. **Should the `?` overlay be completed while we are in it?** It lists `App.tsx`'s five bindings and omits `AgentComposer`'s `Enter` / `Shift+Enter`, so it is not today the complete list of what the app binds. This PRD adds its own row and deliberately does not fix the rest; whether the overlay should be generated from a single registry of bindings rather than a hand-kept array is a small separate question.

## Work Log

### 2026-09-04 — Created

Written from the placeholder in [#744](https://github.com/vfarcic/dot-agent-deck/issues/744) after a reconnaissance pass over `desktop/`, the Tauri and wry sources, and the #803 settings store. Five things the placeholder did not know, four of which changed the plan:

- **The CSS route is not a choice between two mechanisms, it is a 960-literal refactor.** The stylesheet has **zero** `rem` units and 164 all-`px` `font-size` declarations. The issue presents webview zoom and a root font-size as two options; on measurement only one of them exists today.
- **`core:webview:default` does not grant `set_webview_zoom`.** So the frontend route needs a capability widening the app does not currently have, which is what moved the call to a `desktop_set_zoom` app command — where it also serves the launch-time apply.
- **Tauri already ships a zoom-hotkey polyfill, and reading it is the argument against using it.** No persistence, one un-coalesced IPC per keystroke, a 0.2–10.0 range in additive 0.2 steps, no re-fit, no `preventDefault` on keydown, and a completely different implementation on Windows. It is good evidence for the *shape* of the answer and unusable as the answer.
- **The re-fit trigger partly exists already, which is a trap rather than a saving.** Page zoom does shrink the pane's width, so the existing `ResizeObserver` should fire — but `.agent-panel`'s height is clamped at 320px from 1.25× upward, so half the reasoning that made it look automatic is wrong. Relying on it would leave the terminals depending on an implicit path nothing states and no test here can observe.
- **The issue's claim about `App.tsx:167` is stale and narrower than it reads.** The ⌘K branch is at `:232`, the same handler also binds `Escape`, `?`, `1`–`4` and `j`/`k`, and `OutputReader.tsx` registers a second window-level `keydown` listener. The second fact is what makes a third listener ordinary rather than an exception — and the `if (editing) return` at `:241` is what makes putting zoom *in* the existing handler a hazard, since `.xterm-helper-textarea` is focused whenever a pane is.

Two questions rules 9 and 12 would normally raise are answered by precedent: the `experimental` flag is N/A for the whole desktop binary (PRD #176 decision 6), and rule 12 does not apply because nothing here crosses the TUI↔daemon protocol.

### 2026-09-04 — Four decisions settled with the user

Asked before writing, because each one changes the shape of the work rather than its detail.

**Mechanism: webview zoom**, whole window including the terminals. The measurement above is what decided it; the alternatives were a CSS/root-font scale and a terminal-only text size.

**Scope: global, not per-pane.** Recorded as a coupling rather than a preference — webview zoom is a property of the webview, so per-pane is unreachable through the chosen mechanism and would require a second one. Kept as Open Question 2, with the observation that the want behind it may be a layout answer (PRD #313's shipped TUI pane-zoom) rather than a text one.

**Persistence: the existing desktop settings store.** The user's answer was "wherever we store desktop settings", which is `desktop.toml` via PRD #803 — and it is also where the criterion points, since `docs/develop/desktop-gui.md` already lists zoom as client-owned presentational state. No new `localStorage` key, per #824.

**Both surfaces: keybindings *and* a Settings control.** The user asked directly whether it would be both. It is, and they do different jobs: the shortcuts are what you reach for mid-work, and the Settings row is the only place the current level is visible at all — a webview zoom has no indicator of its own. The user also questioned whether the `?` overlay needs a row, on the grounds that `Cmd +/-/0` is a standard everybody knows. Fair, and it still gets one, on the narrower ground that the overlay is where this app's bindings are listed and a reader who opens it should not be told a smaller set than exists. That the overlay is *already* incomplete (it omits the composer's `Enter`/`Shift+Enter`) is now Open Question 5 rather than a reason to skip the row.

### 2026-09-04 — Audited, and two findings changed the design

The draft was audited claim-by-claim against the code before any implementation. 102 claims checked, **16 found wrong, over-stated or unverifiable** — a rate worth recording, since the document was written from direct measurement and still shipped that many. Two of the sixteen were design defects rather than prose defects:

**The keys had to move to the capture phase.** The draft asserted *"there is no key clash to resolve"* on the strength of `App.tsx`'s bindings alone. xterm binds its own `keydown` on `.xterm-helper-textarea`, and its key evaluation maps `Ctrl` + a single-character key with keyCode ≥ 48 to that literal character and `Ctrl _` to 0x1F — so `Ctrl -`, `Ctrl =`, `Ctrl 0` and `Ctrl _` would each have been **typed into the focused agent's PTY** as well as zooming, on Linux and Windows but not macOS (whose `metaKey` misses the branch). A bubble-phase listener cannot undo a byte already sent. The listener is now capture-phase with `stopPropagation()`, there is a Success Criterion for it, a Risk entry, and the descendant-listener test in M3 is the guarantee. This is the clearest case in the document of an absolute hiding a real defect rather than merely overstating a true thing.

**The launch-apply split is now argued rather than asserted.** The draft cited #743's *"no second path that could disagree with this one"* comment as **support** for splitting the apply between Rust (launch) and the frontend (change). Read in full, that comment says the opposite: applying on load *and* on change from one effect is what makes a single path, and the split is the arrangement it was avoiding. The design is unchanged — the flash of unzoomed UI is a real defect the Rust leg removes — but it is now presented as a deliberate departure with the counter-argument first, and with a stated condition under which it should be reverted.

The other fourteen were factual: five `modeScopedKey` keys rather than #824's four (the fifth landed the day after #824 was filed, which is #824's own predicted failure coming true, and made the Success Criterion "no fifth key appears" unsatisfiable as written); the smallest font size is 6px in four places, not 6.5px; the polyfill *does* call `preventDefault()` in its wheel handler, and its steps are additive 0.2 rather than 20%; `.agent-panel` is clamped, so pane height stops tracking the viewport from 1.25× up and "both dimensions shrink" was false; the 3.0 ceiling is a width-and-clipping argument (`overflow-x: hidden`), not a general one; "every size is a fixed pixel value" ignored 148 non-px values; the chrome's range is 6–20px and 13.5px is terminal content rather than chrome; eight of the nine Tauri commands are reached through the bridge seam, not all nine; `AppearancePanel.tsx` is jointly #743's and #803's rather than #743's alone; one quotation was attributed to the docs when it is the registry's docstring; the `?` overlay is not today a complete list of the app's bindings; #869 touches all five shared files rather than four; and two unsourced numbers (key-repeat rate, "nine panes") were removed.
