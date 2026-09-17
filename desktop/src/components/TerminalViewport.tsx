import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";
import type { SendResult, TerminalBuffer, TerminalFeed } from "../types";
import { registerRefit, registerTerminal, unregisterRefit, unregisterTerminal } from "../lib/terminalRegistry";

interface TerminalViewportProps {
  agentId: string;
  /**
   * PRD #1105's security audit — which deck's `agentId` this is.
   *
   * The feed below is addressed by the composite `(deckId, agentId)` because a
   * bare agent id is not an identity: ids are per-daemon monotonic, and leaving
   * a deck detaches its sessions without clearing its retained buffers. A
   * viewport mounted for the next deck's namesake therefore read the previous
   * deck's backlog and wrote it straight into the new xterm — up to the feed's
   * 1 MiB retention of another machine's output, under a correctly resolved
   * heading, and indefinitely where no replacement stream ever arrives.
   *
   * Supplied by `AgentTile` from `agent.daemonId`, so it is the deck of the
   * agent being rendered rather than whichever deck happens to be selected.
   * Optional only because a caller with no fleet at all (the standalone-render
   * tests) has no deck to name; such a caller also supplies no feed.
   */
  deckId?: string;
  label: string;
  transcript: string;
  terminalFeed?: TerminalFeed;
  readOnly?: boolean;
  /**
   * Issue #1042 — the pane's input-acceptance state in the `SendResult`
   * vocabulary, published as `data-input-state` on the wrapper so the state is
   * readable from the DOM rather than inferred from a disabled cursor.
   *
   * Undefined when nothing about the pane is being claimed: the agent-status
   * gate disables the input without saying anything about its lease, and an
   * attribute asserting `applied` there would be a claim this component cannot
   * support. The sentence that goes with either case is rendered by the tile,
   * beside this viewport, so it survives this component being mocked.
   *
   * So this attribute is the pane's CLAIM, not the input's disabled-ness, and
   * the two are different axes: `aria-disabled` on the same wrapper is the
   * authoritative disabled signal and is emitted for every disabling reason,
   * including the status-gate one that leaves this attribute absent. Anything
   * reading `data-input-state` to infer enabled/disabled will misread a
   * status-disabled pane.
   */
  inputState?: SendResult;
  /**
   * Increments when something outside asks this terminal to take focus — the
   * command palette's "Message coordinator…" entry is the only caller today.
   * `onFocus` reports focus outward; this is the way in.
   */
  focusToken?: number;
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
  /**
   * PRD #882 — the geometry the daemon has APPLIED for this agent, or undefined
   * when nothing has said otherwise (a daemon predating the policy, or the
   * browser preview, where the tile's own fit stands).
   *
   * Not the same thing as what this tile asked for: a PTY has one window size,
   * so the daemon sizes the agent to the smallest pane among every client
   * attached and this tile pads the remainder of its box.
   */
  applied?: { rows: number; cols: number };
  onFocus?: () => void;
}

export function TerminalViewport({
  agentId,
  deckId,
  label,
  transcript,
  terminalFeed,
  readOnly,
  inputState,
  focusToken = 0,
  onInput,
  onResize,
  applied,
  onFocus,
}: TerminalViewportProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const terminalRef = useRef<Terminal | undefined>(undefined);
  const lastStreamRef = useRef<TerminalBuffer | undefined>(undefined);

  // The terminal is expensive to build (it allocates a GPU context) and owns
  // scroll position, selection, and cursor state. Anything that changes on
  // every snapshot — the growing transcript, or a callback identity — must be
  // reached through a ref instead of an effect dependency, or the pane is torn
  // down and rebuilt while the operator is typing in it.
  const transcriptRef = useRef(transcript);
  const onInputRef = useRef(onInput);
  const onResizeRef = useRef(onResize);
  // PRD #882: read through a ref for the same reason the callbacks are — the
  // applied geometry changes whenever another client attaches or leaves, and
  // rebuilding the terminal on that would destroy scroll position and selection
  // every time somebody opened the TUI.
  const appliedRef = useRef(applied);
  // Read through a ref for the same reason: a renamed agent must not cost the
  // operator their scroll position and selection.
  const labelRef = useRef(label);
  // Issue #1042 — and read through a ref for the strongest version of that
  // reason. `readOnly` used to be status-only, and a status is effectively
  // monotonic (queued -> running -> passed), so rebuilding on it was rare.
  // It is now also derived from the WRITE LEASE, which is bidirectional and
  // flips during ordinary multi-client operation (PRD #882 hand-off): baking it
  // into the build effect's dependencies tore the pane down and rebuilt it —
  // losing scroll position and any in-progress selection — every time a TUI
  // attached, and again when the lease came back.
  //
  // The ref is what keeps the `onData` guard below honest across that flip: it
  // reads the CURRENT value rather than the one captured when the terminal was
  // built, so a terminal can never announce itself disabled while still
  // accepting keystrokes. The other two seams — `disableStdin` and the helper
  // textarea's native `disabled` — are reconciled by the effect below.
  const readOnlyRef = useRef(readOnly);
  // Set by the terminal effect below so the geometry effect can re-run the
  // grid reconciliation without owning the xterm instance.
  const applyGridRef = useRef<(() => void) | undefined>(undefined);
  transcriptRef.current = transcript;
  onInputRef.current = onInput;
  onResizeRef.current = onResize;
  appliedRef.current = applied;
  labelRef.current = label;
  readOnlyRef.current = readOnly;

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const terminal = new Terminal({
      allowProposedApi: false,
      convertEol: false,
      cursorBlink: !readOnlyRef.current,
      cursorStyle: "bar",
      disableStdin: Boolean(readOnlyRef.current),
      drawBoldTextInBrightColors: false,
      fontFamily: '"JetBrains Mono", "SFMono-Regular", Consolas, monospace',
      fontSize: 13.5,
      fontWeight: "400",
      fontWeightBold: "600",
      lineHeight: 1.3,
      scrollback: 8_000,
      // The embedded terminals stay dark in both app appearances (PRD #743).
      // xterm is fed raw PTY bytes whose colours the agent CLIs chose for a
      // dark background, and truecolor SGR bypasses these 16 slots entirely, so
      // remapping them to the app palette cannot rescue a light pane. Every
      // line below therefore carries the palette guard's opt-out.
      theme: {
        background: "#141817", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        foreground: "#d8ddd8", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        cursor: "#5fc5b5", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        cursorAccent: "#141817", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        selectionBackground: "#3d5652", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        black: "#202524", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        red: "#e5746f", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        green: "#75b890", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        yellow: "#d6ae62", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        blue: "#7ca8bd", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        magenta: "#a89abb", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        cyan: "#65bcb0", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        white: "#d8ddd8", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightBlack: "#717a76", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightRed: "#f08b85", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightGreen: "#8ccc9f", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightYellow: "#e3c17b", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightBlue: "#91bfd2", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightMagenta: "#b9aacd", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightCyan: "#78cec1", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
        brightWhite: "#f3f5f2", // theme-invariant: the terminals stay dark in both appearances (PRD #743)
      },
    });
    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(host);
    // GPU rendering. Without it xterm falls back to the DOM renderer, which
    // cannot keep up with several agents streaming output at once. Loading it
    // must happen after open(); a lost WebGL context degrades to the DOM
    // renderer rather than leaving a dead pane.
    let webglAddon: WebglAddon | undefined;
    try {
      webglAddon = new WebglAddon();
      webglAddon.onContextLoss(() => {
        webglAddon?.dispose();
        webglAddon = undefined;
      });
      terminal.loadAddon(webglAddon);
    } catch {
      webglAddon?.dispose();
      webglAddon = undefined;
    }
    terminalRef.current = terminal;
    // xterm's own `promptLabel` is a module-level string shared by every
    // instance, so the per-agent name has to be written onto this terminal's
    // helper textarea directly. `disabled` goes with it: `disableStdin` only
    // makes xterm ignore what is typed, and an input that still takes focus and
    // a caret while swallowing every keystroke is the void #1042 is about.
    const textarea = terminal.textarea;
    if (textarea) {
      textarea.setAttribute("aria-label", `${labelRef.current} terminal input`);
      textarea.disabled = Boolean(readOnlyRef.current);
    }
    // Expose the instance so the Reader overlay can snapshot the resolved buffer.
    registerTerminal(deckId, agentId, terminal);
    terminal.write(transcriptRef.current);

    const inputDisposable = terminal.onData((data) => {
      if (!readOnlyRef.current) onInputRef.current(data);
    });
    // PRD #882 — `fit()` PROPOSES a size; the daemon disposes.
    //
    // A PTY has exactly one window size, so every client attached to an agent
    // sees the same grid. The daemon sizes each agent to the last-focused
    // client's viewer size, or to the smallest viewport among its attached
    // viewers when no client claimed focus (PRD #1105), which means the grid
    // this tile should render is not necessarily the one that fits its box:
    // with a smaller client deciding it is smaller and the remainder of the box
    // is unused, and with a larger focused client it is larger and clips.
    //
    // So this measures the tile, reports it as a REQUEST, and then puts the
    // grid back to whatever the daemon last applied. Letting `fitAddon.fit()`
    // stand as the authority is what would leave xterm parsing the agent's
    // bytes at this tile's geometry while the PTY is at another client's —
    // absolute cursor positioning landing on the wrong rows, content meant for
    // columns past the edge overprinting the last one. That is PRD #104's
    // mis-parse, relocated from the TUI to here.
    const applyAppliedGrid = () => {
      const applied = appliedRef.current;
      if (!applied) return;
      if (applied.cols < 1 || applied.rows < 1) return;
      if (terminal.cols === applied.cols && terminal.rows === applied.rows) return;
      try {
        terminal.resize(applied.cols, applied.rows);
      } catch {
        // Same defensive posture as `fit` below: a hidden or mid-layout tile
        // can reject a resize, and the next call reconciles it.
      }
    };
    const fit = () => {
      try {
        fitAddon.fit();
        if (terminal.cols > 0 && terminal.rows > 0) onResizeRef.current(terminal.cols, terminal.rows);
        // `fit()` just set the grid to this tile's box. Put it back to the
        // geometry actually in force, if we know one.
        applyAppliedGrid();
      } catch {
        // A hidden/resizing pane can briefly have no measurable dimensions.
      }
    };
    const frame = window.requestAnimationFrame(fit);
    const observer = new ResizeObserver(fit);
    observer.observe(host);
    applyGridRef.current = applyAppliedGrid;
    // PRD #744: a zoom change has to re-fit this pane, and the observer cannot
    // be relied on to do it. Page zoom does shrink the pane's WIDTH, so the
    // observer usually fires — but `.agent-panel`'s height is clamped at 320px
    // and stops moving above 110%, and nothing in a test environment has a
    // layout engine to observe either way. Registering `fit` here is what makes
    // the trigger explicit and testable; the observer stays as the backstop
    // that catches the real post-layout geometry.
    //
    // PRD #882: `fit` reports the tile's box as a REQUEST and then restores the
    // applied grid, so a zoom-driven refit proposes a new size without ever
    // leaving xterm parsing at a geometry the PTY is not using.
    registerRefit(deckId, agentId, fit);

    return () => {
      applyGridRef.current = undefined;
      window.cancelAnimationFrame(frame);
      observer.disconnect();
      inputDisposable.dispose();
      unregisterRefit(deckId, agentId, fit);
      unregisterTerminal(deckId, agentId, terminal);
      webglAddon?.dispose();
      terminal.dispose();
      terminalRef.current = undefined;
      lastStreamRef.current = undefined;
    };
    /*
      `deckId` is a dependency, not a passenger — issue
      [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116)'s open item
      3. This effect built and disposed the xterm on `agentId` alone, so moving
      from deck A's `planner` to deck B's reused the same component, the same
      xterm, the same helper textarea, the same focus and the same
      `lastStreamRef` — the deck changed only which feed was subscribed to.
      B's backlog is then correctly absent, nothing resets the bytes already
      rendered, and the transcript effect below refuses to reset while
      `lastStreamRef.current` still holds A's buffer: A's scrollback stays on
      screen inside B's tile, indefinitely where B has no frame yet or its
      attach failed.

      The cost of having it here is a rebuilt terminal when the identity
      changes, which is exactly right — it is a different agent on a different
      machine, and carrying one pixel of the old one across is the defect.
    */
  }, [agentId, deckId]);

  // Issue #1042 — reconcile the input gate in place when the lease flips.
  //
  // Three seams have to agree, and they are kept in agreement here rather than
  // by rebuilding the terminal (see `readOnlyRef` above for why a rebuild is
  // not acceptable on this input):
  //
  // 1. `options.disableStdin`, which is what makes xterm ignore keystrokes;
  // 2. the `onData` guard in the effect above, which reads `readOnlyRef` so it
  //    can never be one flip behind the other two;
  // 3. the helper textarea's native `disabled`, which is what stops the input
  //    taking focus and showing a caret it would swallow.
  //
  // The wrapper's `aria-disabled` is React's own render below, so it moves with
  // this prop by construction. A seam left behind would produce the one failure
  // worse than the rebuild it replaces: a terminal announcing itself disabled
  // while still accepting what is typed into it.
  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) return;
    const blocked = Boolean(readOnly);
    terminal.options.disableStdin = blocked;
    terminal.options.cursorBlink = !blocked;
    const textarea = terminal.textarea;
    if (textarea) textarea.disabled = blocked;
  }, [readOnly]);

  // A rename changes the accessible name of the input without touching the
  // terminal, so this reconciles the attribute the effect above wrote at
  // construction rather than rebuilding the pane to change one string.
  useEffect(() => {
    const textarea = terminalRef.current?.textarea;
    if (textarea) textarea.setAttribute("aria-label", `${label} terminal input`);
  }, [label]);

  // Issue #1042 — take focus on request. Zero is the never-asked value, so a
  // freshly mounted deck does not steal focus into a terminal nobody named.
  useEffect(() => {
    if (focusToken > 0) terminalRef.current?.focus();
  }, [focusToken]);

  // PRD #882: the daemon changed the applied geometry — because another client
  // attached, detached or resized this agent — so reshape the grid to match.
  // Separate from the terminal effect above so a geometry change reconciles the
  // existing terminal instead of rebuilding it.
  useEffect(() => {
    applyGridRef.current?.();
  }, [applied?.rows, applied?.cols]);

  // A transcript that arrives (or is replaced) before the attach stream has
  // delivered anything still has to reach the screen — but by rewriting the
  // buffer, never by rebuilding the terminal. Once streaming owns the content,
  // the effect below is authoritative and this one stands down.
  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal || lastStreamRef.current) return;
    terminal.reset();
    terminal.write(transcript);
  }, [transcript]);

  // Bytes arrive straight from the bridge feed and go straight into xterm —
  // never through React state, so output volume cannot cause re-renders.
  useEffect(() => {
    if (!terminalFeed) return;
    const apply = (buffer: TerminalBuffer) => {
      const terminal = terminalRef.current;
      if (!terminal) return;
      const previous = lastStreamRef.current;
      if (!previous) {
        if (buffer.data.byteLength) terminal.write(buffer.data);
      } else if (buffer !== previous && buffer.generation === previous.generation) {
        // Compare absolute stream offsets, not array lengths: when the rolling
        // buffer trims its head, baseOffset advances while the tail stays
        // contiguous. Writing just the unseen suffix keeps xterm's scrollback
        // accumulating; the old equal-baseOffset check reset the terminal on
        // every trim, wiping history seconds after it scrolled past.
        const previousEnd = previous.baseOffset + previous.data.byteLength;
        const nextEnd = buffer.baseOffset + buffer.data.byteLength;
        if (nextEnd >= previousEnd && buffer.baseOffset <= previousEnd) {
          const unseen = nextEnd - previousEnd;
          if (unseen > 0) terminal.write(buffer.data.subarray(buffer.data.byteLength - unseen));
        } else {
          // Non-contiguous jump (daemon restart, missed chunks): rebuild.
          terminal.reset();
          terminal.write(transcriptRef.current);
          if (buffer.data.byteLength) terminal.write(buffer.data);
        }
      } else if (buffer !== previous) {
        // Generation changed: the PTY was respawned — a rebuild is correct.
        terminal.reset();
        terminal.write(transcriptRef.current);
        if (buffer.data.byteLength) terminal.write(buffer.data);
      }
      lastStreamRef.current = buffer;
    };
    const backlog = terminalFeed.get(deckId, agentId);
    if (backlog) apply(backlog);
    return terminalFeed.subscribe(deckId, agentId, apply);
  }, [agentId, deckId, terminalFeed]);

  return (
    <div
      className="terminal-viewport"
      data-testid={`terminal-${agentId}`}
      data-input-state={inputState}
      onFocusCapture={onFocus}
      role="group"
      aria-label={`${label} terminal`}
      aria-disabled={Boolean(readOnly)}
    >
      <div ref={hostRef} className="terminal-host" />
    </div>
  );
}
