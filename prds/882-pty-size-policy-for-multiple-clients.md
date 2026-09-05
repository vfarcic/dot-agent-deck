# PRD #882: What the daemon does when clients disagree about PTY size

**Status**: **Decided 2026-09-04 — option 1, daemon owns the size, smallest attached viewer, clients letterbox** (see [Decision taken](#decision-taken)). Implemented in the same pull request, closing both #882 and [#883](https://github.com/vfarcic/dot-agent-deck/issues/883).
**Priority**: High — [#883](https://github.com/vfarcic/dot-agent-deck/issues/883) is blocked on it, and two of the three viable policies change what #883 builds
**Created**: 2026-09-04
**GitHub Issue**: [#882](https://github.com/vfarcic/dot-agent-deck/issues/882)

**Design and decision only. This PRD ships no behaviour change.** Its deliverable is the option set priced against the code, one recommendation, and the decision recorded here for #883 to implement against.

## Problem Statement

A PTY has exactly one window size. The agent process asks the kernel for it (`TIOCGWINSZ`), lays out for that answer, and emits absolute cursor positioning for that grid. Every client attached to that agent therefore sees the same geometry.

Nothing decides what happens when two clients want different geometries. Each asserts its own, and the last writer wins:

- the TUI's per-frame sweep, `resize_panes_to_layout` (`src/ui.rs`), via `resize_pane_pty` (`src/embedded_pane.rs`) → `resize_agent`
- the desktop, sized to its webview tile, via `desktop/src/components/TerminalViewport.tsx` → `desktop/src-tauri/src/terminal.rs` → `resize_agent`

Both land on `AttachRequest::Resize { id, rows, cols }` → `AgentPtyRegistry::resize` (`src/agent_pty.rs`), which ioctls the master. There is no policy and no negotiation.

**The reported symptom is not "last writer wins" — it is "last writer wins, then nobody writes again".** `resize_panes_to_layout` decides whether to send a resize by comparing its layout target against the pane's **own local vt100 parser**, and `resize_pane_pty` sets that parser optimistically and synchronously before the request even reaches the wire (`src/embedded_pane.rs`). So after the desktop moves the PTY, the TUI's target still equals the TUI's parser, the comparison finds no delta, and nothing further goes out until something moves that layout target. `resize_panes_to_layout` is the sole production caller of `resize_pane_pty` (`grep -rn 'resize_pane_pty(' src/` finds one call site outside `#[cfg(test)]`), so there is no second path to correct the daemon in the meantime. Resizing the terminal changes the target, the comparison finally fails, and the pane snaps to full width — exactly the reported "wrong-sized until I resize the terminal". #883 owns that mechanism; it is restated here because the *stickiness* is what makes a policy necessary rather than merely tidy.

### Why PRD #104's cost model needs re-pricing rather than reversing

[PRD #104](104-snapshot-replay-preserves-pty-dims.md) established one-PTY-one-size and the resize-time scrollback-ring clear, and its reasoning is written throughout in terms of "the previous TUI viewport size" — an occasional resize from a user changing their terminal window. It solved TUI-to-TUI reconnect and it solved it correctly.

**Two concurrent clients were not a case it considered.** Nothing in #104 was wrong; its premise changed. What follows re-prices the ring clear under policies where resizes are frequent, and leaves #104's conclusion standing under policies where they are not.

## The constraint, stated once so nobody designs around it

Per-client sizing of a live TUI is **unavailable, not unimplemented**, and the reason has to travel with the claim or the next reader will try to reverse the decision:

- the agent lays out for one `TIOCGWINSZ` answer and emits absolute cursor positioning (`CSI row;col H`) for that grid;
- PRD #104 measured what happens when those bytes are parsed at a different width: cursor positions clamped to the narrower last column, content meant for columns beyond it overprinting that column, spurious wraps inserted, and full-screen redraws landing on wrong rows because the agent's row arithmetic assumed a different height.

**Option 4 in the issue — per-client virtual terminals with server-side re-render — is recorded non-viable for that reason and is not revived here, including as a "maybe later".** 80-column agent output cannot be meaningfully re-rendered at 120 columns; the information the wider layout would need was never emitted.

Everything below therefore lives in one answer space: **pick one size per agent, and have clients adapt rather than compete.**

## Precedent: tmux, measured rather than recalled

tmux has had this problem for twenty years and **does not let clients render at independent sizes.** It picks one size for the window and lets larger clients see unused space. From `man tmux` on tmux 3.6, on this machine:

- **`window-size largest | smallest | manual | latest`** — "If set to `largest`, the size of the largest attached session is used; if `smallest`, the size of the smallest. If `manual`, the size of a new window is set from the `default-size` option … With `latest`, tmux uses the size of the client that had the most recent activity." Note it arbitrates over attached *sessions*, not raw clients.
- **`fill-character`** — "Set the character used to fill areas of the terminal unused by a window." Letterboxing is enough of a first-class outcome in tmux that the pad character is a configuration option.
- **`aggressive-resize`** — resize to the smallest/largest session *for which this is the current window*, rather than every session attached. tmux's own escape hatch for "don't let a client that is not looking at this window constrain it", and it warns the option is "good for full-screen programs which support SIGWINCH and poor for interactive programs such as shells".
- **`resize-window -A` / `-a`** — one-shot "size to the largest / smallest session", which sets `window-size` to `manual` as a side effect.

Measured default on tmux 3.6 here: `window-size` is **`latest`** (`tmux -L … show-options -gv window-size`). The issue records `smallest` as the historical default; that is consistent with tmux's history but was not verified in this analysis, so treat the *option set* as the load-bearing precedent and the default as tmux's own later convenience choice.

**What to take from it:** the four modes are the honest shape of the answer space, and `fill-character` plus `aggressive-resize` say that after two decades the residual problems are *where the dead space goes* and *which viewers get a vote* — not how to render per client.

## The four things established, from the code

### Q1 — Can a client render a grid smaller than its viewport today?

This is the question that makes the letterbox policy either cheap or a rendering project. **The answer differs per client, and the TUI's answer is the surprising one.**

**The TUI: yes, already, and it is already tested.** `TerminalWidget::render` (`src/terminal_widget.rs`) draws `min(inner.height, screen_rows) × min(inner.width, screen_cols)` cells from the top-left, and the cells beyond the screen come out blank on both axes — asserted, not inferred, and asserted against stale content specifically (`tests/render_terminal_widget.rs`: "columns past the PTY width must be blank in the min fallback", "rows past the PTY height must be blank"). Two L1 tests pin exactly this behaviour:

- `render/widget/002` — "rendering a small (e.g. 3×6) PTY screen into a larger (e.g. 6×12) inner area completes without panicking; the PTY content lands at the top-left and the excess rows/columns stay blank".
- `render/widget/003` — a parser at the 4096-column `PTY_RESIZE_DIM_MAX` cap rendered into an inner area two columns wider: "The child's content still renders from the top-left and the columns past the cap stay blank."

And it is not only tested, it **ships**: issue #747's over-cap path is a letterbox in production. `clamp_pane_target_dims` (`src/ui.rs`) deliberately sizes a pane wider than 4096 columns to the cap, and its own comment says the pane "fills 4096 of its columns (the rest rendered blank by `TerminalWidget`'s `min(area, screen)` fallback), which is strictly better than a pane frozen at its previous geometry".

**Two things in the TUI would still have to change, and neither is a rendering change.**

1. `docs/develop/rendering-contract.md` invariant 3 says the widget "assumes the upstream contract holds — the PTY screen is already the size of the inner area in cells", enforced by a live `debug_assert!` comparing the parser against `clamp_pty_dims(inner.height, inner.width)`. Under a policy where the daemon may apply a *smaller* size than this client asked for, that expectation becomes "the screen equals **what the daemon applied**", and the assert has to be given that number or a legitimate letterbox becomes a debug-build panic. Note the shape of the amendment is already in the contract: #747 changed the expectation from "the inner area" to "the inner area capped", for exactly this class of reason.
2. `resize_panes_to_layout`'s "commit only a real delta" check compares the target against the local parser. If the parser holds the daemon's applied size while the target holds this client's larger wish, that check finds a delta on **every frame** and re-sends a resize forever. The comparison has to move to "have I already *asked* for this?" rather than "is my parser this size?". This is the same line #883 is already changing, from the other direction.

**The desktop: no, not today, and the reason is one call site.** `TerminalViewport.tsx` runs `fitAddon.fit()`, which *sets* the xterm grid from the container element and then reports it upward — so the grid equals the viewport by construction, and the daemon is told what the container measured. Letterboxing means the container stops being the authority: `fit()` becomes a *proposal*, and the grid is set from the daemon's applied size. The plumbing for that answer is already there and unused — `DesktopAgent` carries `rows`/`cols` straight off `AgentRecord` (`desktop/src-tauri/src/dto.rs`), and `types.ts` declares them on the frontend type, where no terminal-sizing code reads them.

Two costs attach to the desktop side and both should be decided rather than discovered: the exact xterm entry point for setting a grid explicitly (`FitAddon` itself calls it, but this worktree has no `node_modules`, so the v6 signature is an implementation-time check rather than something verified here), and **where the dead space goes** — `.terminal-host .xterm { height: 100% }` (`desktop/src/styles.css`) stretches the element to the tile today, so an undersized grid leaves unused area at the bottom-right in the terminal background unless the CSS is asked to do something else. tmux's `fill-character` exists because this is a real choice, not a detail.

**Verdict: the letterbox policy is not a rendering project.** One client already does it, with tests; the other needs one call site inverted and one CSS decision.

### Q2 — Does the chosen policy need a protocol change?

**What already exists, so it is not re-bought:** `AgentRecord` carries the daemon's `pty_rows`/`pty_cols` as `rows`/`cols` (PRD #104), `list_agents` returns it, and both clients receive it — the TUI at hydration (`parser_init_dims(record.rows, record.cols)`, `src/embedded_pane.rs`) and the desktop on every snapshot refresh.

**Three findings that decide the cost, in order of how much they matter.**

**1. Adding an optional field is free; adding a frame kind or an event variant is not.** `AttachResponse` (`src/daemon_protocol.rs`) is a flat bag of `#[serde(default, skip_serializing_if = "Option::is_none")]` fields, and its own doc comments record the precedent repeatedly — `server_version`, `build_version`, `agents_summary` were each added without a `PROTOCOL_VERSION` bump. Echoing the applied `(rows, cols)` on the `Resize` response is that same pattern, and `DaemonClient::resize_agent` currently reads the response, checks `ok`, and discards the rest — so the channel exists and is being thrown away.

**2. An unsolicited geometry *push* is a hard wire break, on either available channel.** This is the one genuinely expensive protocol finding, and it is measurable from two call sites:

- `AttachConnection::next_output` (`src/daemon_client.rs`) matches `KIND_STREAM_OUT` and `KIND_STREAM_END` and treats **any other frame kind as end of stream** — it warns and returns `Ok(None)`. A new `KIND_GEOMETRY` frame would therefore make an older client's pane go dead on the first push.
- `EventSubscription::next_event` returns `io::ErrorKind::InvalidData` on a `KIND_EVENT` payload it cannot deserialize, which drops the subscription. `src/event.rs` already records this for `BroadcastMsg`: adding a variant "changes the `KIND_EVENT` payload schema (an older peer would mis-parse the new `kind` tag), so it bumps `PROTOCOL_VERSION`".

So a policy that must tell an *incumbent* viewer "your agent's geometry just changed because someone else attached" either takes a `PROTOCOL_VERSION` bump plus a real interop failure for older clients, or it settles for the clients asking. And per `PROTOCOL_VERSION`'s own docs, no call site refuses on a mismatch today (issue #405) — the build-version handshake's decline path attaches with no version check of any kind — so the bump would *name* the skew rather than prevent it.

**3. The compatible substitute for a push is a poll, and its latency is not symmetric between the clients.** `list_agents` needs no wire change at all. The desktop effectively already polls: `ensure_snapshot_watcher` (`desktop/src-tauri/src/lib.rs`) refreshes the full snapshot — a `ListAgents` round trip — on daemon events, coalesced to one per `SNAPSHOT_COALESCE_INTERVAL` (150 ms). **But it is event-driven, not periodic**, so for a busy agent the applied dims arrive sub-second and for a silent agent they may not arrive at all. The TUI has no steady-state poll at all — its `list_agents` calls are hydration, a close-slot lookup, a reattach-lookup retry loop and the handshake, every one of them triggered by an event rather than a clock — so it would need a refresh trigger it does not have today.

**Per-policy summary.** Rule 12's cross-version manual test applies to every row that is not "none", with both documented false-green traps: a previous-release daemon started with **no live agents** silently gets terminated and replaced by the branch TUI, and more than the 30-second `DEFAULT_IDLE_SHUTDOWN_SECS` between `daemon serve` and the first attach does the same. Exactly one `Attach protocol listening` line for the whole run is the tell for both; export `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`, and isolate `DOT_AGENT_DECK_LOG` and `DOT_AGENT_DECK_EXPERIMENTAL` along with the sockets, `HOME` and the state dir.

| policy | wire change | `PROTOCOL_VERSION` | `.breaking.md` |
| --- | --- | --- | --- |
| 1 — daemon owns, smallest attached, clients letterbox | none *required* (see Q3 — peer PID + an additive `Resize`-response echo), or a bump if incumbents must be *pushed* rather than poll | no, unless the push channel is chosen | **yes** — `Resize` becomes a request the daemon may decline, which is a semantic break behind a stable wire |
| 2 — freeze while more than one client is attached | additive only (the applied-dims echo, so a frozen-out client's parser does not diverge) | no | **yes** — same reason: `Resize` may now do nothing |
| 3 — latest-attach wins, made explicit | **none** | no | no — the wire and its meaning are unchanged; only the client-side comparison moves |

### Q3 — What re-evaluates the size when a client detaches?

**`client_count` (`src/daemon.rs:340`) is a socket count, not a client count, and cannot carry either of the policies that need one.** Read from the code:

- `serve_attach_with_counter` (`src/daemon_protocol.rs`) increments it **per accepted connection**, via an RAII guard, and decrements on that connection's task ending.
- `handle_connection` reads exactly **one** frame and dispatches one request, so every `Resize`, `ListAgents`, `SetAgentLabel` and `WriteAndSubmit` is its own short-lived connection. `DaemonClient::resize_agent`'s doc says so outright: "each call opens a fresh short-lived connection".
- `DaemonClient::attach` opens **one connection per agent**, and `subscribe_events` opens another for the client as a whole.

So a single TUI showing three agent panes holds four or more connections before any RPC traffic. And the count's only consumer is `run_idle_monitor`, which compares it to zero — a value above 1 has never meant anything to anything.

**The number that does exist per agent is `AgentPtyRegistry::receiver_count(id)`** (`src/agent_pty.rs`), which forwards to `broadcast::Sender::receiver_count()` — the live `AttachStream` subscribers for that one agent. It is already load-bearing elsewhere in the registry. That is a genuine per-agent viewer count, and it is the right input to "is more than one client watching *this* agent". A count is still not a set of geometries.

**No client identity exists on the attach protocol** — `grep -rn "client_id" src/` returns nothing, and no request carries one. But identity is **derivable today with zero protocol bytes**, and this is the finding that most changes the price of options 1 and 2: `src/platform/peercred/` already wraps peer-PID discovery for all three platforms — `getsockopt(SO_PEERCRED)` on Linux, `LOCAL_PEERPID` on macOS, `GetNamedPipeClientProcessId` on Windows — and the Windows backend explicitly implements and documents the **server-end** direction ("`Server` | the connected client | `GetNamedPipeClientProcessId`"). Every connection from one client process therefore shares a PID, including the short-lived `Resize` connections, so the daemon can attribute a resize to a *client* without a wire change.

**But a PID identifies a client, not a viewer, and the geometry map needs the viewer** (raised by Greptile on PR #884; the first draft of this section presented PID plus stream teardown as sufficient, and it is not). Two things break a PID-keyed map. A `Resize` arrives on its **own** short-lived connection, so the PID says which process asked and nothing about which of that process's views it asked for; and one process can hold more than one `AttachStream` for the same agent — two desktop tiles showing it, or a tile plus the Reader overlay — at which point two different tile geometries collapse onto one key and the later one silently wins. The prune is wrong in the same way: the first of those streams to end would delete an entry the other still needs.

So the honest division of labour is **peer PID for grouping, a per-attach key for geometry**. Getting a per-attach key means the size travels on the long-lived connection — the additive `AttachStream { id, rows, cols }` fields, `#[serde(default)]`, no bump — or the `Resize` carries a token the attach handed out. Peer PID keeps its value for the questions it can actually answer: telling one client's connections from another's, and noticing that a whole client is gone. Anything option 1 does with per-viewer geometry rests on the per-attach key, not on the PID.

Three caveats belong with that, because the mechanism is coded rather than exercised in this direction:

- **Today's only callers hold the client end.** `daemon_stop` and `build_version_handshake` use `peer_pid` to identify the *daemon*; the Windows module says so explicitly. The server-end direction is implemented and unit-asserted, but has no production caller — so it needs verifying on all three platforms, not assuming.
- **PID reuse is a known hazard here already**, and `build_version_handshake` carries a mitigation for it (re-resolve on the same held-open stream).
- **macOS's re-resolve fails after peer close** — recorded in that same code path. Capturing the PID at accept, while the stream is open, is the shape that works.

**Under either constraining policy the re-evaluation trigger is the same, and it already exists**: the per-agent `AttachStream` connection's end. The `AttachHandle` returned by `AgentPtyRegistry::subscribe` is already the per-viewer object, and its drop is the detach signal that `receiver_count` reads. So "release the constraint when the small client leaves, and re-apply the smallest of what remains" is expressible without new plumbing.

**One consequence to decide rather than discover: the desktop's attach set tracks what is on screen.** `setShownTerminals`' contract in `desktop/src/lib/bridge.ts` is explicit — "Attach follows this and nothing else … an attach costs one daemon socket and one full scrollback replay per agent". Under a per-viewer constraint, an agent's geometry therefore changes when a desktop tile scrolls into or out of view, which is churn under a policy whose selling point is stability. That is the sub-decision recorded as Open Question 1.

### Q4 — Re-pricing the ring clear

`AgentPtyRegistry::resize` drops the daemon's scrollback ring after a successful ioctl (PRD #104 M3) so a replay snapshot spans one dimension epoch. Its guard skips the whole ioctl-plus-bookkeeping path only when **neither** dimension changes, and two clients rarely agree.

**Be precise about what it costs, because it is narrower than it sounds.** `AgentBus::clear_scrollback` leaves live subscribers untouched: an already-attached client keeps its own vt100 parser scrollback and can still scroll it. What is lost is the **replay for a client attaching afterwards** — it gets a correct live screen with no history behind it. #104's rationale that the SIGWINCH redraw "repopulates scrollback at the new dims within the first frame" holds for the live screen and not for history.

| policy | resizes per attention switch | who pays |
| --- | --- | --- |
| 1 — smallest attached | zero. The size changes when the *set of viewers* changes, not when attention moves | #104's reasoning stands unchanged |
| 2 — freeze while >1 attached | zero while frozen; one when the freeze lifts | cheapest on this axis, by declining to serve anybody's resize |
| 3 — latest-wins | one per agent per switch, once #883 makes a client re-assert on becoming active | a later attacher, and see below |

**Under latest-wins the "later attacher" is a routine user action, not an edge case.** Because the desktop attaches per *shown* tile, the sequence "work in the TUI (the switch resizes and clears the rings), then scroll a desktop tile back into view" replays an emptied ring into that tile. #883 states this trade correctly and it is worth restating as the reason it cannot be split: today's stale comparison means the TUI does *not* re-resize on a switch, so there is no ioctl and no ring clear — **the bug is currently masking the cost.** Fixing the comparison under latest-wins converts "the pane looks wrong until I resize" into "the pane looks right and the next attach has no history".

## The options, priced

| | 1 — daemon owns, smallest attached, clients letterbox | 2 — freeze while >1 client attached | 3 — latest-attach wins, made explicit |
| --- | --- | --- | --- |
| every client sees the agent's **whole** screen | **yes** — the PTY is never larger than any viewport, so every client is on the safe `min` fallback with blank padding | not necessarily — whoever attached first sets it | **no** — a client smaller than the PTY renders a truncated top-left window |
| stability | size changes only when the viewer set changes | frozen | changes on every attention switch |
| ring clear | rare (#104 stands) | none while frozen | one per agent per switch |
| protocol | additive, or a bump for a push channel | additive | **none** |
| rendering work | TUI: contract amendment + delta check. Desktop: one call site + a CSS decision | none | TUI: none. **Desktop: one call site + a CSS decision anyway** — see the correction below |
| rule 12 fragment | `.breaking.md` (`Resize` may be declined) | `.breaking.md` (same) | none |
| main objection | a small viewer constrains everyone | the chosen size is arbitrary, and a user's own terminal resize silently does nothing | part of the agent's output becomes unreachable in the smaller client |

**The one asymmetry that decides it: latest-wins does not avoid the letterbox requirement, it just chooses the dangerous half of it.** Under any single-size policy a client whose viewport differs from the PTY must render a grid that is not its viewport. There are two directions, and they are not equivalent:

- **PTY smaller than the viewport** — the client draws the entire agent screen and pads the remainder. Ugly, complete, already implemented and tested in the TUI.
- **PTY larger than the viewport** — the client draws `min(area, screen)` from the top-left, so the agent's bottom rows and right-hand columns exist but are off-screen with no way to reach them. For a full-screen agent that is its status line and its right-hand column: Claude Code's footer simply vanishes.

Smallest-attached makes the padding direction the steady state — a client that has just attached and not yet declared its geometry can still be transiently undersized, which is a first-frame case rather than a resting one. Latest-wins makes the truncating direction normal.

**Correction to this table, found while recording the decision: option 3's rendering work is not zero.** The first draft of the row above said "none" for option 3 on the strength of the TUI, which is correct for the TUI and wrong for the desktop. Latest-wins does not exempt a client from rendering a grid that is not its viewport — it only removes the *policy* machinery. Once the daemon's size can differ from a client's tile (which is the whole premise), the desktop has two choices and only one of them is acceptable: keep `fit()` as the authority and let its xterm grid diverge from the PTY, which is precisely the PRD #104 mis-parse in the desktop rather than the TUI; or set the grid from the daemon's applied size, which lands it in both directions — padding when the PTY is smaller than the tile, and clipping when it is larger, because `.terminal-viewport` is `overflow: hidden` (`desktop/src/styles.css`), so the lost right-hand columns go silently. **What option 3 genuinely saves is the protocol work, the rendering-contract amendment, the `.breaking.md` and the per-viewer accounting — not the desktop's grid-authority change.** That change is needed under every policy in this document, which is a reason to treat it as #883's first milestone rather than as any one policy's price.

## Recommendation

**Option 1 — the daemon owns the size, sized to the smallest attached viewer, and clients letterbox.**

Four reasons, in the order they carry weight:

1. **It is the only option under which no part of an agent's output becomes unreachable in any client** (the asymmetry above). That is a functional property, not an aesthetic one.
2. **Its main objection describes today's behaviour, and option 1 is the fix for it.** "A small viewer pins every agent small" is what already happens: the desktop's tile size is pushed to the daemon and *sticks* there, which is the bug report that opened this. The difference option 1 makes is that the constraint is **deterministic** (the smallest, not whoever moved last) and **released on detach** (hide the tile and the agent grows back), instead of arbitrary and sticky until someone resizes a terminal by hand.
3. **The rendering half is mostly already built.** The TUI letterboxes in production and has two L1 tests pinning it; the desktop's unused `AgentRecord.rows/cols` are already on the wire and in its frontend types.
4. **It is #819's principle applied to geometry** — "the daemon owns the world, the client owns only its settings" — so it is the one option that does not have to be re-argued the next time a client is added. #742's fleet view is that next time.

**Why the others lose.**

**Option 2 loses on its own premise.** Its claim to being the cheapest real fix rests on `client_count`, and that number cannot express "more than one client" (Q3): one TUI with three panes already reads four or more. The count that *can* — `receiver_count(id)` — needs the same per-agent viewer accounting option 1 needs, so option 2 pays most of option 1's cost and answers with an arbitrary size. And its failure mode is the least actionable of the three: while frozen, a user dragging their own terminal window gets nothing, and there is no rule they can act on to change that. Option 1 has a milder relative of the same thing — enlarging your terminal past the smallest viewer gives you padding rather than a bigger agent — but that follows a stated rule with a remedy (detach or hide the other viewer), where a first-attacher freeze offers neither.

**Option 3 loses on the truncation direction, not on cost.** It is genuinely the cheapest — no protocol change, no contract amendment, no per-viewer accounting, and #883 close to as already scoped (the desktop's grid authority still has to move; see the correction above) — and it is a strict improvement on today, because chosen-and-documented beats emergent. It should be the fallback if the letterbox work is judged too expensive for the payoff. But it makes "part of the agent's screen is unreachable in this client" a normal state, and it pays a ring clear per switch into a desktop that re-attaches on tile visibility.

**What #883 builds under each answer**, so the handoff is unambiguous:

- **Option 1** — the client stops asserting geometry and starts requesting it; the daemon keys a per-agent geometry map **per attach** (the size riding on `AttachStream`, or a token the attach hands out — *not* by peer PID, see Q3), applies the smallest, prunes when that attach ends, and echoes the applied size on the `Resize` response; the TUI's invariant-3 expectation and delta check move to the applied size; the desktop sets its xterm grid from the applied size instead of from `fit()`.
- **Option 2** — the client keeps asserting, conditional on a viewer count it must be told; the daemon declines a resize while more than one viewer is attached and echoes the size it kept; something re-evaluates on the last detach.
- **Option 3** — #883 close to as already scoped: stop comparing the target against the local parser, re-assert **on client activity rather than on attach** (see [The trigger](#the-trigger-which-the-policy-name-does-not-settle) — attach-only would self-correct almost never), accept the ring clear, and move the desktop's grid authority off `fit()` onto the daemon's applied size (the one item the original scope did not carry — see the correction above). Then document both newly-normal behaviours: a pane that shows part of its agent's screen, and an empty replay for a client attaching after a size change.

## Scope

### In scope

- The policy decision, the option set priced against the code, and the four questions above answered with call sites.
- Where the mechanism and the user-visible consequence get documented (below).
- The handoff to #883.

### Out of scope

- Any behaviour change. No client-side or daemon-side code moves in this PRD.
- Option 4 (per-client virtual terminals with server-side re-render) — recorded non-viable above.
- The client-switch defects themselves — #883.
- A user-facing *setting* for the policy. The issue is explicit that this ends with one policy chosen, not three behind a toggle. tmux's `window-size` exists because tmux is a general-purpose multiplexer with two decades of installed habit; picking one rule is the cheaper answer until someone reports needing another.

## Documentation

Per rule 11, the mechanism is developer-facing and the consequence is user-facing:

- **`docs/develop/rendering-contract.md`** (not published) — the size policy belongs here, because invariants 2 and 3 are what it amends. Invariant 2 currently reads "PTY size is derived from the layout rect, not pushed by event handlers"; under option 1 it becomes "the client *requests* from its layout rect; the daemon decides". Invariant 3's expectation moves from "the inner area, capped" to "what the daemon applied". Worth noting while editing that file: its "Out of scope / known caveats" still says "Stream-backed (daemon) panes have no PTY resize op. `resize_panes_to_layout` silently skips them", which PRD #76 M2.10 has since made false — `resize_pane_pty` handles stream panes by coalescing to the daemon. Correcting that is a one-line fix for whoever amends the file next, and is not done here.
- **`docs/troubleshooting.md`** (published) — the user-visible consequence, which is that a pane may not fill its area while another client is attached to the same agent. It reads as a bug unless it is written down, and that page already carries the neighbouring "window too small for the cards" explanation. `docs/session-management.md` is the alternative home if the wording lands better as expected multi-client behaviour than as a symptom.

Both are #883's to write, alongside the change they describe. **Under the chosen policy the user-facing half is the larger of the two**, because two behaviours become expected rather than exceptional and both look like defects: a pane that shows only part of its agent's screen while a differently-sized client is attached, and scrollback replay that is empty for a client attaching after a size change. Whether the truncating case should also *say* something in the client — a marker on the pane, a hint line — is a real question and belongs to #883 rather than here; `overflow: hidden` on the desktop and `min(area, screen)` in the TUI both lose the content silently today.

## Success criteria

This PRD is done when all of the following hold. They are deliberately about the *decision*, since no behaviour changes here.

1. **Met.** One of options 1–3 is chosen by the maintainer and recorded — option 3, in [Decision taken](#decision-taken).
2. **Met.** The three answers #883 depends on are unambiguous from this document: a client keeps **asserting** geometry, the daemon **applies** every request it receives (latest wins, no arbitration, no decline), and **no protocol change is in scope**.
3. **Met.** Open Question 1 is answered, and recorded as moot under the chosen policy.
4. **Met on this PRD's side.** #883 is unblocked and its scope is the one it already carries; restating it on the issue is #883's own first step.

## Risks

Marked by whether they apply under the chosen policy (option 3) or only to a future revisit of option 1.

| Risk | Applies | Impact | Mitigation |
| --- | --- | --- | --- |
| The server-end `peer_pid` direction has no production caller today | option 1 only | Option 1's zero-wire-change client identity is coded and unit-asserted but unexercised in this direction; a platform surprise would push it toward a wire change after all | Verify at the top of #883's implementation, on all three platforms, before the design depends on it. The additive `AttachStream { id, rows, cols }` fields are the fallback and cost no bump |
| PID reuse or a macOS re-resolve failure misattributes a viewer's geometry | option 1 only | Wrong size applied, or a stale constraint never released | Capture at accept while the stream is open (the shape `build_version_handshake` already uses); the `AttachStream` end is the authoritative prune, not the PID's liveness |
| A desktop grid of many small tiles pins every agent small under option 1 | option 1 only | Agents laid out for a monitoring tile while a full-screen TUI user is driving them | Open Question 1 — decide whether a viewer that is only monitoring constrains the size. Note the same shrink already happens today, and stickily |
| The invariant-3 `debug_assert!` is given the wrong expectation | option 1 only | Every debug build panics on a legitimate letterbox | It is a single comparison with one production caller; `render/widget/003` is the existing test for exactly this class of change and is the place to extend |
| The ring clear is under-communicated | **live** | "The pane looks right and my scrollback is gone" — silent, which is worse than visible | #883 must document the ring clear as user-visible behaviour, not only as a code comment |
| The desktop keeps `fit()` as its grid authority under option 3 | **live** | The PRD #104 mis-parse relocates from the TUI to the desktop: xterm parses PTY bytes at the tile's geometry while the PTY is at whichever client acted last | #883's first milestone is the desktop's grid authority, independent of policy — see the correction in the options section |
| Truncation is silent in both clients | **live** | A user sees a pane that looks complete and is not; the agent's status line is simply absent | Decide in #883 whether the oversize case gets a visible marker. `min(area, screen)` in the TUI and `overflow: hidden` on the desktop both lose it without a word today |

## Open questions

1. **Do monitoring viewers get a vote?** **Answered "yes" (2026-09-04), and moot under the chosen policy — recorded for a future revisit of option 1.** Under option 1, does a desktop tile that is showing a terminal but is not the one being driven constrain the agent's geometry? tmux's `aggressive-resize` is precedent for "no" — size to the sessions for which this is the *current* window — and it also warns that the option suits full-screen SIGWINCH-aware programs and not shells, which is a fair description of the split between agents and plain panes here. Answering "no" costs a declared-intent bit per viewer and keeps a nine-tile overview from shrinking every agent. Answering "yes" is simpler and matches tmux's plain `smallest`. **Recommendation: start with "yes" (plainest rule, no new concept), and treat "no" as the first thing to add if the overview grid proves it necessary — the geometry map is keyed per viewer either way, so the bit can be added later without redesigning.**
2. **Does the desktop letterbox to the top-left or centre the grid in the tile, and what happens when the grid is *larger* than the tile?** **Still live under the chosen policy** — see the correction in the options section: latest-wins removes the policy machinery, not the grid-authority change. The TUI's answer is fixed by `TerminalWidget` (top-left, and `min` in the oversize direction). The desktop is a CSS decision in both directions, and `.terminal-viewport`'s `overflow: hidden` currently makes the oversize direction a silent clip. tmux's `fill-character` says the pad is worth an explicit choice.
3. **Is a floor needed?** **Moot under the chosen policy** in its "smallest" form, but the underlying hazard survives: latest-wins means a single briefly-measured tile mid-layout can move every attached client to that geometry until the next switch. A viewer that declares an absurdly small geometry — a briefly-measured tile mid-layout, a one-column window — would pin the agent there under "smallest". `PTY_RESIZE_DIM_MAX` bounds the top end; nothing bounds the bottom beyond `rows == 0 || cols == 0` being rejected. Deferred to #883 unless the maintainer wants a number decided here.

## Decision taken

**Option 1 — the daemon owns the size, sized to the smallest attached viewer, and clients letterbox — chosen by the maintainer on 2026-09-04.** This PRD and the implementation ship in one pull request, closing both #882 and #883.

**This reverses an earlier decision taken the same day, and the reversal is the useful part of the record.** Option 3 (latest wins) was chosen first, while the implementation was still going to be a separate PR. When implementation moved into this PR, the calculus changed: option 1's extra cost — per-viewer accounting, a `.breaking.md`, the rendering-contract amendment, rule 12's cross-version manual test — stopped being deferred work in somebody else's issue and became work in the same review as everything else, which is the cheapest moment to pay it. The option-3 record is kept below rather than deleted, because the next person to weigh these three options should see that the answer moved on scheduling rather than on evidence.

The recommendation above is left standing as written for the same reason it was when it disagreed with the decision.

**What the decision accepts:**

- **A small viewer constrains everyone.** With Open Question 1 answered "yes, every attached viewer constrains", a desktop tile showing an agent sizes that agent for a full-screen TUI user too. This is the known cost, and the mitigation is real but partial: it is *deterministic* (the smallest, not whoever moved last) and *released on detach* (hide the tile and the agent grows back), where today the same shrink happens arbitrarily and sticks until someone resizes a terminal by hand. If the overview grid proves this intolerable in use, the fix on record is Open Question 1's other answer — a declared-intent bit per viewer — which the per-viewer map is already keyed to accept.
- **A `PROTOCOL_VERSION` bump and a new frame kind.** Incumbent viewers have to be told when the applied size changes because somebody else joined or left, and Q2 established there is no compatible push channel. The break is contained by making the push **opt-in at attach**: a client that declares its geometry on `AttachStream` is a policy participant and receives geometry frames; one that does not is legacy, receives none, and its stream cannot be broken by a frame kind it has never been sent.
- **`Resize` becomes a request the daemon may only partly honour** — a semantic break behind a stable wire, so a `.breaking.md` fragment, and rule 12's cross-version manual test in full.
- **Dead space in the larger client.** The TUI already renders it (blank, top-left anchored, two L1 tests); the desktop needs its grid authority moved off `fit()` and a decision about where the unused area goes.

**What it buys:** every client always sees the agent's entire screen. That is the one property no other option on the table has, and the reason it was recommended.

**Open Question 1 is answered "yes — every attached viewer constrains"**, and under this policy it is load-bearing rather than moot.

**Superseded: the option-3 decision, recorded 2026-09-04 and reversed the same day.** It read: *"Option 3 — latest-attach wins, made explicit."* What it accepted was the truncating direction becoming normal (a client smaller than the PTY renders a top-left window, so a full-screen agent's status line sits off-screen) and a ring clear per agent per attention switch; what it bought was no protocol change, no `.breaking.md`, no contract amendment and no per-viewer accounting. Its trigger analysis is **not** superseded and applies to this policy too — see [The trigger](#the-trigger-which-the-policy-name-does-not-settle), which is why that section is kept below.

**If this decision is ever reversed, the argument to beat is the direction asymmetry**, not the cost: every single-size policy makes some client render a grid that is not its viewport, and this one puts the loss where space goes unused rather than where content becomes unreachable.

### The trigger, which the policy name does not settle

Raised by Greptile on PR #884, and it is a real gap rather than a wording quibble: "latest-**attach** wins" and "the client that most recently became active" are different events, and #883's scoped fix says "re-assert on attach/focus" while this PRD's cost model assumes a resize per attention switch. Those cannot both be right, because **neither client re-asserts on focus today, and for the TUI "focus" barely exists**: it is a terminal program with no notion of its window being frontmost, and it stays attached the whole time the desktop is being used. Attach-only would therefore self-correct almost never — the TUI does not re-attach when you switch back to it.

**The trigger is client activity, and tmux's own wording is the authority for it**: `window-size latest` uses "the size of the client that had the most recent **activity**". Read that way the policy is *most-recent-activity wins*, and both clients can detect activity locally with no new information — a keystroke delivered into a pane, a pane-focus change, a tile becoming shown. That also makes the re-assert unconditional rather than a comparison, which sidesteps the awkward fact established in Q2 that the TUI has no steady-state poll and so cannot compare against the daemon's dims on demand.

**One sub-decision is left for #883, stated so it is chosen rather than defaulted: what a *passive* client renders while the other one is driving.** Re-asserting on activity fixes the client you are using, and leaves the one you are only watching parsing at its own geometry while the PTY sits at the other's — mis-parsed until your next interaction with it. Two ways out, and they differ in cost rather than in kind:

- **Accept the transient.** No protocol change at all. The residual is a window between the other client winning and your next activity in this one — much shorter than today's "sticky until you resize your terminal by hand", which is the bug being fixed.
- **Take the applied-dims echo** on the `Resize` response (additive optional field, no bump — Q2 has the precedent) plus a refresh trigger, so a passive client can re-parse at the daemon's geometry without asserting anything. Strictly better, and the point at which "no protocol change" becomes "one additive field".

Recommendation: start with the first, because it is the policy as chosen and it is measurable in use; add the echo if watching an idle pane while driving the other client proves to be a real workflow. Either way **the trigger itself is activity, not attach** — that is the part #883 should not re-derive.

## Work Log

### 2026-09-05 — Greptile review: four real defects, and the live test earned its keep

Six findings on PR #895. Four were genuine defects in this implementation, all of the same family — a geometry that reaches a parser at the wrong moment:

- **Hydration ignored the applied geometry.** `wire_stream_pane` seeded the parser from the caller's dims (an `AgentRecord` read *before* the attach, or the pane's layout target) while the snapshot about to be replayed was taken under the same daemon lock as `applied`. Attaching is itself what registers a viewer and can shrink the agent, so the two legitimately differ — and parsing those bytes at the caller's dims is PRD #104's scramble reached through the new path. Now `conn.applied()` wins, fixed inside `wire_stream_pane` so every call site is covered rather than the two hydration ones.
- **A lagged geometry receiver was disabled permanently.** `Err(_)` conflated `Lagged` with `Closed`, so a viewer that fell behind the 16-entry broadcast stopped receiving geometry for the life of the connection — and a passive viewer has no poll to recover with. `Lagged` now keeps the receiver, which a tokio broadcast supports.
- **A stale viewer token fell through to the unattributed path.** Attaches and resizes travel on separate connections, so a resize can legitimately land after its attach ended. Treating that as a legacy override applied it directly, bypassing every remaining viewer's minimum with nothing to recompute afterwards — an idle viewer has no reason to send another resize, so the PTY could stay larger than an attached pane indefinitely. A supplied-but-unknown token is now ignored; only an *absent* token takes the legacy path.
- **The desktop applied geometry asynchronously and delivered the replay synchronously.** Notifying the listeners only schedules a React state update, while the buffered replay is written to xterm immediately — so the replay was parsed at the tile's fitted grid whenever it differed from the applied one, and the damage sticks because the daemon's snapshot is a single epoch that will not be re-sent. Both the attach path and the push path now resize the live instance synchronously through the existing terminal registry.

**The live test was run, and it failed — which is exactly why the rule exists.** Greptile pointed out that writing a credentialed test and not running it is not what rule 5 asks for. Credentials were available, so it ran: the first version sent `Ctrl+D` before typing, which *leaves* pane input for command mode, so the directive went to the deck instead of the agent and no work ever happened. A broken lane-2 test that nobody runs is worse than no test, and only running it surfaced that. Fixed, and it now passes against a live Haiku — with a guard asserting the sentinel is absent *before* the directive, so a pass cannot come from the deck's own chrome.

### 2026-09-05 — A CI failure was taken seriously and found a real regression

`e2e-deterministic` failed on `manager_016_wheel_over_dialog_does_not_scroll_side_pane` — a scheduler mouse-wheel test with no connection to PTY sizing, in a lane that is known-red. It would have been easy, and wrong, to file that as the known flake: a survey of the twelve most recent failing runs showed `orchestration_remit_*` in eleven of them and **`manager_016` in none** — and that family passed in this run. A first-time failure in a test that compares rendered frames across a layout change, in a PR that changes when a pane's geometry moves, is not a coincidence to wave through.

**The defect was mine, and the test was pointing at something worse than itself.** The first cut removed the optimistic parser write from `resize_pane_pty` entirely, reasoning that since the daemon decides the geometry the client should wait to be told. That is wrong for the case that dominates: with one client attached the daemon grants exactly what was asked, so waiting buys nothing and costs a full round trip of lag on *every* resize — the pane renders its old grid inside its new box until the answer lands, and its geometry then changes at a moment unrelated to the frame that caused it.

The fix is optimistic-then-corrected: write the requested geometry immediately, and let the daemon's answer (or a `KIND_GEOMETRY` push) correct it. The single-client path is byte-for-byte what it was, and in the multi-client case the transient is in the **safe** direction — the parser is briefly larger than the PTY, so bytes land where they were drawn, with stale columns to the right until the SIGWINCH redraw. It does not reintroduce issue #883, whose bug was the per-frame sweep *comparing* against the parser; that comparison now reads `requested`.

`resize_layout_002` gets its parser assertion back as a result, so issue #747's invariant is once again pinned on both halves — the clamped request and the clamped optimistic write — rather than only the one the client still owned under the first cut.

### 2026-09-05 — Option 1 implemented; four things the design did not anticipate

Built the policy: the daemon keys a per-agent geometry map **per attach**, applies the smallest viewport on each axis, prunes on attach teardown, echoes the applied geometry on both the attach and resize responses, and pushes changes to participating viewers as a new `KIND_GEOMETRY` frame. `PROTOCOL_VERSION` 8 → 9, with a `.breaking.md` fragment for the semantic change to `Resize`. The TUI stops asserting geometry and sizes its parser from the daemon's answer; the desktop's `fit()` becomes a proposal and its xterm grid is set from the applied geometry.

Four things the design did not anticipate, all found while building it:

- **The push cannot be avoided, and the design section understated this.** Q2 priced a push as optional ("or it settles for the clients asking"). It is not optional: when a viewer *detaches*, the minimum grows, and every remaining viewer's parser is instantly the wrong shape for the bytes arriving. No remaining viewer has any reason to ask, and the TUI has no steady-state poll — so without the push, option 1 would introduce the PRD #104 mis-parse by exactly the mechanism meant to prevent it. What made the bump acceptable is narrower than "we bumped": the push is **opt-in at attach**, so a client that predates the frame kind is never sent one and cannot have its pane torn down by a frame `next_output` would read as end-of-stream.
- **Registering a viewer must happen under the same lock as the snapshot, in that order.** Registering can shrink the agent, and a resize clears the scrollback ring — so taking the snapshot first hands the client bytes written at the old geometry together with a token telling it to parse at the new one. That is issue #686's hazard reached by a different route, and it is why `subscribe_with_viewport` does both under one lock rather than composing two public calls.
- **Greptile's per-attach correction earned its keep immediately.** The map is keyed per attach, not per client, and the prune runs on the attach task's drop — which also covers a client that crashes rather than detaching cleanly. A PID key would have leaked a constraint pinning a live agent to the geometry of a pane nobody was looking at any more.
- **Invariant 3 does not survive in any form, not even a one-directional one.** The PRD predicted its expectation would "become what the daemon applied". That was wrong in an interesting way: the parser *is* what the daemon applied, so there is nothing left to compare it against. And because every request is now a round trip, the parser is transiently larger than the pane during a shrink and smaller during a grow — both indistinguishable from a defect at a call site that cannot know whether an answer is in flight. The assertion was removed and the invariant moved to the daemon, which is the side that can state it. `docs/develop/rendering-contract.md` records this.

Also corrected while in the file: that document's "stream-backed panes have no PTY resize op" caveat, stale since PRD #76 M2.10 and flagged in this PRD's Documentation section.

### 2026-09-04 — Greptile review round: four findings, three of them real gaps

Greptile raised four P2 findings on PR #884. All four were valid at review time; three needed changes to this document and one had already been fixed in a follow-up commit Greptile could not have seen (it reviews once, when a PR opens — `greptile.json` sets `triggerOnUpdates: false`).

- **The resize trigger was ambiguous, and this was the substantive one.** "Latest-**attach** wins" and "the client that most recently became active" are different events, and this PRD's cost model assumed the second while #883's scope said the first. Attach-only would self-correct almost never, because the TUI stays attached the whole time the desktop is in use and has no notion of window focus at all. Resolved by naming **activity** as the trigger, on tmux's own authority — `window-size latest` uses "the size of the client that had the most recent activity" — and by recording the one sub-decision that falls out of it (what a passive client renders while the other drives) as an explicit fork for #883 with both costs, rather than leaving it to be defaulted. New subsection under Decision taken.
- **Peer PID identifies a client, not a viewer.** The option-1 sketch keyed the per-agent geometry map by peer PID and pruned on stream teardown, which does not hold: a `Resize` arrives on its own connection so the PID cannot say *which* view it is for, one process can hold two attaches for the same agent (two tiles, or a tile plus the Reader overlay), and the first stream to end would delete an entry the other still needs. Corrected in Q3 to "peer PID for grouping, a per-attach key for geometry", and the option-1 handoff updated. This is option-1 material and not the chosen path, but the record is explicitly kept for a future revisit, so a wrong sketch there is a trap rather than a curiosity.
- **The Work Log's closing line contradicted the decision.** Struck through and marked superseded rather than deleted — the entry was accurate when written and a Work Log is a record of what was true then.
- **"Option 3 needs no rendering work" — already corrected** in `13c69ad`, before the review was read. Greptile reviewed `e3d955b`.

### 2026-09-04 — Decision taken: option 3, latest-attach wins

The maintainer chose **option 3 — latest-attach wins, made explicit**, over the recommended option 1. Recorded in [Decision taken](#decision-taken) with what it accepts (the truncating direction becomes normal; a ring clear per agent per switch) and what it buys (no protocol change, no `.breaking.md`, no rendering-contract amendment, no per-viewer accounting, and #883 ships as scoped). The recommendation is left standing as written rather than edited into agreement.

Open Question 1 was answered "yes, every attached viewer constrains" and is moot under this policy; it is kept on record for any future revisit of option 1.

**One claim of this PRD's own did not survive recording the decision, and is corrected rather than quietly dropped.** The options table said option 3 needs no rendering work. That is true of the TUI and false of the desktop: latest-wins removes the policy machinery, not the fact that a client must render a grid that is not its viewport. The desktop either keeps `fit()` as the authority and lets its xterm grid diverge from the PTY — the PRD #104 mis-parse, relocated — or sets the grid from the daemon's applied size and handles both directions, where the oversize one is currently a silent clip (`.terminal-viewport` is `overflow: hidden`). The correction is in the options section and in Open Question 2. It does not overturn the decision — the protocol, contract, fragment and accounting savings are all real — but it moves the desktop's grid-authority change out of "option 1's price" and into "#883's first milestone under any policy".

`cargo fmt --check` and `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` both clean.

### 2026-09-04 — Design complete; four questions answered from the code

Answered the issue's four questions against `main`, priced the three viable options, and recommended option 1. Findings that changed the pricing relative to the issue's own framing:

- **The TUI already letterboxes, in production, with two L1 tests** (`render/widget/002`, `render/widget/003`) — so option 1's rendering requirement is met on one client and is one call site plus a CSS decision on the other. This is the load-bearing answer and it moved option 1 from "possibly a rendering project" to "mostly already built".
- **`client_count` is a socket count, not a client count.** `handle_connection` serves exactly one request per connection and `attach` opens one connection per agent, so a three-pane TUI reads four or more; its only consumer compares it to zero. Option 2's claim to cheapness does not survive this. The usable number, `receiver_count(id)`, already exists per agent.
- **Client identity is derivable with zero protocol bytes.** `src/platform/peercred/` implements peer-PID discovery on all three platforms and the Windows backend documents the server-end direction explicitly — so "smallest attached client" is expressible today, which the issue had listed as an open matter. Caveat recorded: that direction has no production caller yet.
- **An unsolicited geometry push is a hard break on either channel** — `next_output` treats an unknown frame kind as end-of-stream, and `next_event` errors out on an unparseable `KIND_EVENT` payload. The compatible substitute is the existing `list_agents`, plus an additive applied-dims echo on the `Resize` response, which `resize_agent` currently discards.
- **The decisive argument is a direction, not a cost.** Every single-size policy needs a client to render a grid that is not its viewport; smallest-attached makes the padding direction the only reachable one, while latest-wins makes the truncating direction normal — and truncation puts a full-screen agent's status line off-screen for as long as the PTY stays larger than the pane.

tmux precedent read from `man tmux` on tmux 3.6 rather than recalled, including `fill-character` and `aggressive-resize`, and its default measured as `latest` on this machine.

`cargo test-fast` — 2401 tests, all passed.

~~**Outstanding: the policy decision itself.** #883 stays blocked until it is recorded here.~~ — **superseded the same day** by the entry above: option 3 was chosen and #883 is unblocked. Left struck through rather than deleted, since a Work Log is a record of what was true when written.
