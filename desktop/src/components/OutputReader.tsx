import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowDownToLine, BookOpenText, Copy, X } from "lucide-react";
import type { AgentSession } from "../types";
import { getTerminal, stripAnsi, terminalSnapshotText } from "../lib/terminalRegistry";
import { blockedReasonText } from "../lib/blockedReason";

/** How often the reader re-snapshots the live terminal buffer while open. */
const REFRESH_MS = 700;
/** "Near the bottom" slack, in px, before live-tail unpins. */
const PIN_SLACK = 48;

interface OutputReaderProps {
  agent: AgentSession;
  onClose: () => void;
}

/**
 * A large, readable rendering of an agent's CLI output. Text comes from the
 * live xterm buffer snapshot (repaints already resolved), soft-wrapped lines
 * re-joined so the column reflows at reading width. Live-tails while pinned to
 * the bottom; scrolling up pauses the tail until "Jump to latest".
 *
 * Read-only, which is what its construction always said it was: a snapshot
 * `<pre>`, "Copy all", and a live-tail pin, with no terminal of its own. It
 * carried an `AgentComposer` until issue #1042, which was this surface's ONLY
 * send path rather than a duplicate of one — and a second, independent input
 * box floating in a reading overlay is exactly the parallel-input surface that
 * issue exists to collapse. The send path is the tile's terminal, one Escape
 * away.
 */
export function OutputReader({ agent, onClose }: OutputReaderProps) {
  const [text, setText] = useState("");
  const [pinned, setPinned] = useState(true);
  const [copied, setCopied] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  pinnedRef.current = pinned;
  // Read through a ref so the snapshot interval sees the current transcript
  // without re-arming on every snapshot update.
  const transcriptRef = useRef(agent.transcript);
  transcriptRef.current = agent.transcript;

  useEffect(() => {
    const snapshot = () => {
      /* By the composite identity: two decks' same-id agents can both have a
         live xterm since PRD #1105's cross-deck pane, and a bare-id lookup
         would snapshot whichever mounted last under either agent's heading. */
      const terminal = getTerminal(agent.daemonId, agent.id);
      setText(terminal ? terminalSnapshotText(terminal) : stripAnsi(transcriptRef.current));
    };
    snapshot();
    const timer = window.setInterval(snapshot, REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [agent.daemonId, agent.id]);

  // Follow new output only while the reader is pinned to the bottom.
  useEffect(() => {
    const pane = scrollRef.current;
    if (pane && pinnedRef.current) pane.scrollTop = pane.scrollHeight;
  }, [text]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const handleScroll = useCallback(() => {
    const pane = scrollRef.current;
    if (!pane) return;
    setPinned(pane.scrollHeight - pane.scrollTop - pane.clientHeight < PIN_SLACK);
  }, []);

  const jumpToLatest = useCallback(() => {
    const pane = scrollRef.current;
    if (pane) pane.scrollTop = pane.scrollHeight;
    setPinned(true);
  }, []);

  const copyAll = useCallback(() => {
    void navigator.clipboard?.writeText(text).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1_500);
    });
  }, [text]);

  return (
    <div className="reader-overlay" data-testid={`reader-${agent.id}`} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="reader-panel" role="dialog" aria-modal="true" aria-label={`${agent.role} output reader`}>
        <header className="reader-head">
          <div className="reader-title">
            <BookOpenText size={15} aria-hidden="true" />
            <div>
              <strong>{agent.role}</strong>
              <span className={`status-label status-${agent.status}`}>{agent.status}</span>
              {/* Issue #714: the reason, where the status is. */}
              {agent.status === "blocked" && <span className="agent-blocked-reason">{blockedReasonText(agent.blocked)}</span>}
            </div>
          </div>
          <div className="reader-controls">
            <span className={`reader-tail ${pinned ? "is-live" : ""}`}>{pinned ? "LIVE TAIL" : "PAUSED"}</span>
            <button className="button compact" onClick={copyAll} title="Copy the full output as text">
              <Copy size={12} aria-hidden="true" /> {copied ? "Copied" : "Copy all"}
            </button>
            <button className="button compact" aria-label="Close reader" onClick={onClose}><X size={13} aria-hidden="true" /></button>
          </div>
        </header>

        <div className="reader-scroll" ref={scrollRef} onScroll={handleScroll}>
          <pre className="reader-text">{text || "No output yet."}</pre>
        </div>

        {!pinned && (
          <button className="reader-jump" onClick={jumpToLatest}>
            <ArrowDownToLine size={13} aria-hidden="true" /> Jump to latest
          </button>
        )}
      </div>
    </div>
  );
}
