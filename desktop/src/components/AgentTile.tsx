import { useCallback, useEffect, useState } from "react";
import {
  AlertTriangle,
  BookOpenText,
  Box,
  Check,
  CheckCircle2,
  CircleDot,
  FileCode2,
  GitCompareArrows,
  Handshake,
  Pencil,
  ShieldCheck,
  SquareTerminal,
  X,
} from "lucide-react";
import { UNREPORTED } from "../types";
import type {
  AgentPanePresentation,
  AgentSession,
  EvidenceItem,
  PanelTab,
  RuntimeMode,
  SendResult,
  TerminalFeed,
} from "../types";
import { terminalInputState } from "../lib/terminalInput";
import { OutputReader } from "./OutputReader";
import { TerminalViewport } from "./TerminalViewport";

const tabs: { id: PanelTab; label: string; icon: typeof SquareTerminal }[] = [
  { id: "terminal", label: "Terminal", icon: SquareTerminal },
  { id: "diff", label: "Diff", icon: GitCompareArrows },
  { id: "checks", label: "Checks", icon: ShieldCheck },
  { id: "handoffs", label: "Handoffs", icon: Handshake },
  { id: "artifacts", label: "Artifacts", icon: Box },
];

interface AgentTileProps {
  agent: AgentSession;
  /**
   * PRD #1105 M1 — which of this component's two presentations to render.
   *
   * Required rather than defaulted to `"tile"`, so a new render site has to
   * say which of the two it is instead of inheriting one silently. There is
   * exactly one render site today (`App.tsx`'s `.agent-grid`), so the cost of
   * requiring it is one line.
   *
   * The four differences M1 established, recorded here because this prop is
   * where a later implementer will look for them:
   *
   * 1. **Panel tabs — all five at BOTH sizes.** The tab set is a property of
   *    the agent, not of the box. `tab` is controlled by `App` and keyed by
   *    agent id, so the overlay inherits the tile's tab and hands it back on
   *    close; a presentation that dropped tabs would have to coerce that
   *    shared state behind the user's back. What the presentation DOES change
   *    is the strip's labels — today they are shown by a `min-width: 1450px`
   *    media query, which asks about the WINDOW when the question is how wide
   *    this pane is.
   * 2. **Header — same content, relaxed clamps.** Everything in the header
   *    identifies the agent, and identity matters MORE in the overlay, where
   *    the grid position that used to say which agent you are looking at is
   *    gone. So nothing is removed; `.agent-assignment`'s two-line clamp is
   *    relaxed because it exists for a half-width tile. The close control is
   *    NOT derived from this prop — closing acts on the view, not on the
   *    agent, so it arrives as an optional `onClose` callback gating an
   *    affordance, the pattern `onRename` already uses here.
   * 3. **The terminal's box — CSS only, no measurement change.**
   *    `TerminalViewport` measures its host with `FitAddon` under a
   *    `ResizeObserver`, so a bigger box reports a bigger geometry through the
   *    path that already exists. `"overlay"` drops `.agent-panel`'s fixed
   *    `height: 42vh` band for `flex: 1 1 auto`. Note the overlay must
   *    override the tile's media queries too, `max-width: 680px`'s
   *    `.agent-panel, .agent-tabs { display: none }` above all — an overlay
   *    that inherited it would show no terminal at all.
   * 4. **The Reader launcher — `"tile"` only.** `OutputReader` binds `Escape`
   *    on `window`, and so will the overlay; two listeners on the same target
   *    both fire, so one `Escape` inside the overlay would close the Reader
   *    AND the overlay. Beyond that the overlay already IS reading width, with
   *    colour and a cursor. Hiding the button is not sufficient on its own:
   *    `readerOpen` is local state reset only on `agent.id`, so the overlay
   *    must also not RENDER `<OutputReader>`.
   *
   * What this prop deliberately CANNOT express: that the tile and the overlay
   * share one xterm instance. That is a property of where the element sits in
   * the React tree, not of what it is rendered with — which is why M3 has to
   * promote the tile's element in place rather than re-parent it.
   */
  presentation: AgentPanePresentation;
  mode: RuntimeMode;
  selected: boolean;
  tab: PanelTab;
  terminalFeed?: TerminalFeed;
  evidence: EvidenceItem[];
  /**
   * Issue #1042 — the last non-delivered verdict the guarded send verb returned
   * for this agent, if any. The only condition #1042 names that the snapshot
   * cannot express (`wrong-session`) arrives this way and no other.
   */
  inputResult?: SendResult;
  /** Increments when the command palette asks this tile's terminal to focus. */
  terminalFocusToken?: number;
  onSelect: () => void;
  onTabChange: (tab: PanelTab) => void;
  onTerminalInput: (agentId: string, data: string) => Promise<void>;
  onTerminalResize: (agentId: string, cols: number, rows: number) => Promise<void>;
  /** PRD #882 — the geometry the daemon has applied for this agent, if known. */
  appliedGeometry?: { rows: number; cols: number };
  onEvidenceSelect: (id: string) => void;
  onRename?: (agentId: string, displayName: string) => Promise<void>;
}

function formatTokens(tokens: number): string {
  return tokens >= 1_000 ? `${(tokens / 1_000).toFixed(1)}k` : String(tokens);
}

export function AgentTile({
  agent,
  presentation,
  mode,
  selected,
  tab,
  terminalFeed,
  evidence,
  inputResult,
  terminalFocusToken,
  onSelect,
  onTabChange,
  onTerminalInput,
  onTerminalResize,
  appliedGeometry,
  onEvidenceSelect,
  onRename,
}: AgentTileProps) {
  const handleInput = useCallback((data: string) => {
    void onTerminalInput(agent.id, data);
  }, [agent.id, onTerminalInput]);
  const handleResize = useCallback((cols: number, rows: number) => {
    void onTerminalResize(agent.id, cols, rows);
  }, [agent.id, onTerminalResize]);
  const agentEvidence = evidence.filter((item) => agent.handoffIds.includes(item.id) || item.agentId === agent.id);
  // Issue #1042: one derivation for the terminal's state, its read-only gate
  // and the sentence beside it, so the three cannot disagree the way the tile's
  // inline status condition and the composer's own copy of it could.
  const input = terminalInputState(agent, inputResult);
  const fixture = mode === "fixture";
  const [renameDraft, setRenameDraft] = useState<string>();
  const [readerOpen, setReaderOpen] = useState(false);
  useEffect(() => { setRenameDraft(undefined); setReaderOpen(false); }, [agent.id]);

  const commitRename = () => {
    const next = renameDraft?.trim();
    setRenameDraft(undefined);
    if (!onRename || !next || next === agent.displayName) return;
    void onRename(agent.id, next);
  };

  return (
    <article
      className={`agent-tile ${selected ? "is-selected" : ""}`}
      data-testid={`agent-tile-${agent.role.toLowerCase().replaceAll(" ", "-")}`}
      data-status={agent.status}
      /* The seam the presentation differences key off. An attribute selector
         outranks a bare class even inside a media query, so
         `[data-presentation="overlay"]` can override the tile's own responsive
         rules with no stylesheet reordering. Nothing matches it yet. */
      data-presentation={presentation}
      onMouseDown={onSelect}
    >
      <header className="agent-header">
        <div className="agent-identity">
          <span className={`agent-state-mark status-${agent.status}`} aria-hidden="true" />
          <div>
            <div className="agent-title-line">
              <h2>{agent.role}</h2>
              {agent.isStartRole && <span className="coordinator-badge" title="Orchestration start role">COORDINATOR</span>}
              <span className={`status-label status-${agent.status}`}>{agent.status}</span>
            </div>
            {renameDraft !== undefined ? (
              <div className="agent-rename" onMouseDown={(event) => event.stopPropagation()}>
                <input
                  autoFocus
                  aria-label={`Rename ${agent.role}`}
                  value={renameDraft}
                  onChange={(event) => setRenameDraft(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") { event.preventDefault(); commitRename(); }
                    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); setRenameDraft(undefined); }
                  }}
                />
                <button aria-label={`Save ${agent.role} name`} onClick={commitRename}><Check size={12} /></button>
                <button aria-label={`Cancel renaming ${agent.role}`} onClick={() => setRenameDraft(undefined)}><X size={12} /></button>
              </div>
            ) : (
              <p>
                {/* Issue #856: the binary the daemon named, dropped entirely
                    when it named none — the separator goes with it rather than
                    leaving a dangling `· Unavailable`. */}
                {agent.cli ? <>{agent.cli} <span aria-hidden="true">·</span> </> : null}{agent.model}
                {onRename && (
                  <button
                    className="agent-rename-trigger"
                    aria-label={`Rename ${agent.role}`}
                    title={`Rename ${agent.displayName}`}
                    onMouseDown={(event) => event.stopPropagation()}
                    onClick={() => setRenameDraft(agent.displayName)}
                  ><Pencil size={11} /></button>
                )}
              </p>
            )}
          </div>
        </div>
        {/*
          PRD #745 M8: an em dash — this deck's established "not known" — when
          no attempt was reported. Live mode used to hardcode `1`, so every tile
          printed `ATT 01` as if the daemon tracked retries; it tracks none.
        */}
        <div className="agent-attempt" title={agent.attempt === undefined ? "No attempt count is reported by the deck" : "Current attempt"}>
          <span>ATT</span>
          <strong>{agent.attempt === undefined ? "—" : agent.attempt.toString().padStart(2, "0")}</strong>
        </div>
      </header>

      <div className="agent-assignment">
        <span>ASSIGNMENT</span>
        <p>{agent.task}</p>
      </div>

      <div className="agent-instruments" aria-label={`${agent.role} run metrics`}>
        <div><span>TIME</span><strong>{agent.duration}</strong></div>
        {fixture ? (
          <>
            <div><span>TOKENS</span><strong>{formatTokens(agent.tokens)}</strong></div>
            <div><span>COST</span><strong>${agent.cost.toFixed(2)}</strong></div>
            <div><span>CONTEXT</span><strong>{agent.contextPercent}%</strong></div>
          </>
        ) : (
          <>
            <div><span>TOOLS</span><strong>{agent.toolCount}</strong></div>
            <div><span>MODEL</span><strong>—</strong></div>
            <div><span>USAGE</span><strong>—</strong></div>
          </>
        )}
      </div>

      <div className="agent-tabs" role="tablist" aria-label={`${agent.role} details`}>
        {tabs.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            className={tab === id ? "is-active" : ""}
            role="tab"
            aria-selected={tab === id}
            aria-label={label}
            title={label}
            onMouseDown={(event) => event.stopPropagation()}
            onClick={() => onTabChange(id)}
          >
            <Icon size={13} aria-hidden="true" />
            <span>{label}</span>
            {id === "checks" && agent.checks.length > 0 && <em>{agent.checks.length}</em>}
          </button>
        ))}
        <button
          className="reader-open"
          data-testid={`reader-open-${agent.id}`}
          title="Open a large readable view of this agent's output"
          onMouseDown={(event) => event.stopPropagation()}
          onClick={() => setReaderOpen(true)}
        >
          <BookOpenText size={13} aria-hidden="true" />
          <span>Reader</span>
        </button>
      </div>

      {readerOpen && <OutputReader agent={agent} onClose={() => setReaderOpen(false)} />}

      <div className="agent-panel" role="tabpanel">
        {tab === "terminal" && (
          <div className="agent-terminal-stack">
            <TerminalViewport
              agentId={agent.id}
              label={agent.role}
              transcript={agent.transcript}
              terminalFeed={terminalFeed}
              readOnly={input.readOnly}
              inputState={input.state}
              focusToken={terminalFocusToken}
              onInput={handleInput}
              onResize={handleResize}
              applied={appliedGeometry}
              onFocus={onSelect}
            />
            {input.notice && (
              <p
                className={`terminal-input-status is-${input.tone}`}
                data-testid={`terminal-input-status-${agent.id}`}
                role="status"
              >
                <AlertTriangle size={12} aria-hidden="true" />
                {input.notice}
              </p>
            )}
          </div>
        )}
        {tab === "diff" && (
          <div className="text-panel diff-panel">
            <div className="panel-caption"><FileCode2 size={14} /> Changed files</div>
            {agent.diff.length ? agent.diff.map((line) => <code key={line}>{line}</code>) : <EmptyPanel label={fixture ? "No changes in this agent's lease" : "Diff data is not exposed by the deck"} />}
          </div>
        )}
        {tab === "checks" && (
          <div className="text-panel checks-panel">
            {agent.checks.length ? agent.checks.map((check) => (
              <div className="check-row" key={check.id}>
                {check.status === "passed" ? <CheckCircle2 size={15} /> : <CircleDot size={15} />}
                <div><strong>{check.name}</strong><span>{check.command ?? "Pending command"}</span></div>
                <small>{check.duration ?? check.status}</small>
              </div>
            )) : <EmptyPanel label={fixture ? "No checks attached" : "Check results are not exposed by the deck"} />}
          </div>
        )}
        {tab === "handoffs" && (
          <div className="text-panel handoff-panel">
            {agentEvidence.length ? agentEvidence.map((item) => (
              <button key={item.id} onClick={() => onEvidenceSelect(item.id)}>
                <span className={`verdict verdict-${item.verdict.toLowerCase()}`}>{item.verdict}</span>
                <div><strong>{item.title}</strong><small>{item.from} → {item.to}</small></div>
              </button>
            )) : <EmptyPanel label={fixture ? "No handoff evidence yet" : "Structured handoffs are not exposed by the deck"} />}
          </div>
        )}
        {tab === "artifacts" && (
          <div className="text-panel artifact-panel">
            {agent.artifacts.length ? agent.artifacts.map((artifact) => (
              <div key={artifact.id}><Box size={14} /><span><strong>{artifact.name}</strong><code>{artifact.path}</code></span></div>
            )) : <EmptyPanel label={fixture ? "No artifacts produced" : "Artifacts are not exposed by the deck"} />}
          </div>
        )}
      </div>

      <footer className="agent-footer">
        <span>{fixture ? `LEASE · ${agent.writeLease.toUpperCase()}` : "LEASE · UNAVAILABLE"}</span>
        {/*
          The deck's own word for a working directory the daemon did not report.
          The model says `undefined` rather than carrying the word, because the
          daemon can legitimately report a directory NAMED "Unavailable" and a
          sentinel it can spell is one it can forge (M8 audit); the substitution
          lives here, where it is printed, so nothing matches on it. No `title`
          in that case — there is no path to put in one.
        */}
        <span title={agent.cwd}>{fixture ? agent.worktree : agent.cwd ?? UNREPORTED}</span>
        {agent.activeTool && <strong>{agent.activeTool}</strong>}
      </footer>
    </article>
  );
}

function EmptyPanel({ label }: { label: string }) {
  return <div className="panel-empty"><CircleDot size={15} /><span>{label}</span></div>;
}
