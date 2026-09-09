import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";
import type { TerminalBuffer, TerminalFeed } from "../types";
import { registerRefit, registerTerminal, unregisterRefit, unregisterTerminal } from "../lib/terminalRegistry";

interface TerminalViewportProps {
  agentId: string;
  label: string;
  transcript: string;
  terminalFeed?: TerminalFeed;
  readOnly?: boolean;
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
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
  transcriptRef.current = transcript;
  onInputRef.current = onInput;
  onResizeRef.current = onResize;

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
    const fit = () => {
      try {
        fitAddon.fit();
        if (terminal.cols > 0 && terminal.rows > 0) onResizeRef.current(terminal.cols, terminal.rows);
      } catch {
        // A hidden/resizing pane can briefly have no measurable dimensions.
      }
    };
    const frame = window.requestAnimationFrame(fit);
    const observer = new ResizeObserver(fit);
    observer.observe(host);
    // PRD #744: a zoom change has to re-fit this pane, and the observer cannot
    // be relied on to do it. Page zoom does shrink the pane's WIDTH, so the
    // observer usually fires — but `.agent-panel`'s height is clamped at 320px
    // and stops moving above 110%, and nothing in a test environment has a
    // layout engine to observe either way. Registering `fit` here is what makes
    // the trigger explicit and testable; the observer stays as the backstop
    // that catches the real post-layout geometry.
    registerRefit(agentId, fit);

    return () => {
      window.cancelAnimationFrame(frame);
      observer.disconnect();
      inputDisposable.dispose();
      unregisterRefit(agentId, fit);
      unregisterTerminal(agentId, terminal);
      webglAddon?.dispose();
      terminal.dispose();
      terminalRef.current = undefined;
      lastStreamRef.current = undefined;
    };
  }, [agentId, readOnly]);

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
