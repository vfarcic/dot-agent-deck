import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";
import type { TerminalBuffer, TerminalFeed } from "../types";
import { registerTerminal, unregisterTerminal } from "../lib/terminalRegistry";

interface TerminalViewportProps {
  agentId: string;
  label: string;
  transcript: string;
  terminalFeed?: TerminalFeed;
  readOnly?: boolean;
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
  label,
  transcript,
  terminalFeed,
  readOnly,
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
  // Set by the terminal effect below so the geometry effect can re-run the
  // grid reconciliation without owning the xterm instance.
  const applyGridRef = useRef<(() => void) | undefined>(undefined);
  transcriptRef.current = transcript;
  onInputRef.current = onInput;
  onResizeRef.current = onResize;
  appliedRef.current = applied;

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const terminal = new Terminal({
      allowProposedApi: false,
      convertEol: false,
      cursorBlink: !readOnly,
      cursorStyle: "bar",
      disableStdin: readOnly,
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
    // Expose the instance so the Reader overlay can snapshot the resolved buffer.
    registerTerminal(agentId, terminal);
    terminal.write(transcriptRef.current);

    const inputDisposable = terminal.onData((data) => {
      if (!readOnly) onInputRef.current(data);
    });
    // PRD #882 — `fit()` PROPOSES a size; the daemon disposes.
    //
    // A PTY has exactly one window size, so every client attached to an agent
    // sees the same grid. The daemon sizes each agent to the smallest viewport
    // among its attached viewers, which means the grid this tile should render
    // is not necessarily the one that fits its box: with a smaller client
    // attached it is smaller, and the remainder of the box is unused.
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

    return () => {
      applyGridRef.current = undefined;
      window.cancelAnimationFrame(frame);
      observer.disconnect();
      inputDisposable.dispose();
      unregisterTerminal(agentId, terminal);
      webglAddon?.dispose();
      terminal.dispose();
      terminalRef.current = undefined;
      lastStreamRef.current = undefined;
    };
  }, [agentId, readOnly]);

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
    const backlog = terminalFeed.get(agentId);
    if (backlog) apply(backlog);
    return terminalFeed.subscribe(agentId, apply);
  }, [agentId, terminalFeed]);

  return (
    <div
      className="terminal-viewport"
      data-testid={`terminal-${agentId}`}
      onFocusCapture={onFocus}
      role="group"
      aria-label={`${label} terminal`}
    >
      <div ref={hostRef} className="terminal-host" />
    </div>
  );
}
