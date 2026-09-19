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
  Maximize2,
  Pencil,
  ShieldCheck,
  SquareTerminal,
  Unplug,
  X,
} from "lucide-react";
import { UNREPORTED } from "../types";
import type {
  AgentPanePresentation,
  AgentSession,
  AgentTarget,
  EvidenceItem,
  PanelTab,
  RuntimeMode,
  SendResult,
  TerminalFeed,
} from "../types";
import { terminalInputState, type AgentRecordFreshness, type NoTerminalState } from "../lib/terminalInput";
import { OutputReader } from "./OutputReader";
import { TerminalViewport } from "./TerminalViewport";

const tabs: { id: PanelTab; label: string; icon: typeof SquareTerminal }[] = [
  { id: "terminal", label: "Terminal", icon: SquareTerminal },
  { id: "diff", label: "Diff", icon: GitCompareArrows },
  { id: "checks", label: "Checks", icon: ShieldCheck },
  { id: "handoffs", label: "Handoffs", icon: Handshake },
  { id: "artifacts", label: "Artifacts", icon: Box },
];

export interface AgentTileProps {
  agent: AgentSession;
  /**
   * PRD #1105 M1 — which of this component's two presentations to render.
   *
   * Required rather than defaulted to `"tile"`, so a new render site has to
   * say which of the two it is instead of inheriting one silently. M3 made
   * `App.tsx`'s `AgentPaneFrame` the only thing that renders this component,
   * at either presentation, so the prop is set in exactly one place and the
   * cost of requiring it stayed one line.
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
   *    **This difference is necessary and was not sufficient**, which is the
   *    correction the PR review made. It governs the tile being promoted and
   *    says nothing about the other tiles the pane is drawn over, which stay at
   *    `"tile"` and keep any Reader they already had — a second `window`
   *    listener, behind the scrim, answering the same `Escape`. The screen-wide
   *    half is {@link AgentTileProps.panePresent}, and the two are deliberately
   *    separate: one is a fact about this box, the other a fact about the
   *    screen, and only the first is a presentation.
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
  /**
   * PRD #1105 — set when this pane has no terminal at all, and why.
   *
   * Absent means one is attached, which is every tile on the deck and every
   * overview-origin pane whose agent is on the selected deck. Set, the terminal
   * tab renders the state and its sentence INSTEAD of a `TerminalViewport`:
   * mounting one that will receive no bytes paints a black rectangle reading
   * "this agent is producing no output", which is false about an agent working
   * normally elsewhere. See {@link NoTerminalState} for why it is not a value
   * inside the `SendResult` input vocabulary.
   *
   * A capability-shaped prop rather than a `presentation` branch, for the
   * reason {@link AgentTileProps.onOpen} is one: it describes what this render
   * site can honour, and only a caller that can see both the agent's deck and
   * the selected one is able to answer it.
   */
  noTerminal?: NoTerminalState;
  /**
   * Issue #1143 — whether {@link AgentTileProps.agent} is this deck's current
   * answer or the last one it gave, defaulting to `"live"` because every other
   * render site in the app builds its records from a snapshot that just
   * arrived.
   *
   * `"held"` changes what the header ASSERTS, and only that. The status is the
   * field that lies hardest — `running` beside a live dot is a claim about
   * *now*, and the record may be arbitrarily old — so it reads `last seen:
   * running` and drops its live colour. The heading, the role and the identity
   * are left alone: they are what the agent IS rather than what it is doing,
   * and they do not decay the way a status does.
   */
  recordFreshness?: AgentRecordFreshness;
  /** Increments when the command palette asks this tile's terminal to focus. */
  terminalFocusToken?: number;
  onSelect: () => void;
  onTabChange: (tab: PanelTab) => void;
  /**
   * Both take the COMPOSITE identity since PRD #1105's cross-deck pane: this
   * tile's agent may be on a deck that is not the selected one, and an id alone
   * names an agent on every deck (`AgentTarget`).
   */
  onTerminalInput: (target: AgentTarget, data: string) => Promise<void>;
  onTerminalResize: (target: AgentTarget, cols: number, rows: number) => Promise<void>;
  /** PRD #882 — the geometry the daemon has applied for this agent, if known. */
  appliedGeometry?: { rows: number; cols: number };
  onEvidenceSelect: (id: string) => void;
  onRename?: (agentId: string, displayName: string) => Promise<void>;
  /**
   * PRD #1105 M5 — enlarge this agent, supplied only by a parent that owns the
   * view. A capability rather than a `presentation` branch, for the reason
   * `onRename` is one: the affordance exists exactly when somebody can honour
   * it, and a tile rendered by a caller with no navigation (every standalone
   * `ControlDeck` in the tests) must not offer a control that does nothing.
   */
  onOpen?: () => void;
  /**
   * True while an agent pane is open ANYWHERE in this screen — including over
   * a different agent's tile.
   *
   * This is not `presentation === "overlay"` and cannot be derived from it. The
   * promoted tile renders no Reader, but the tiles it is drawn over stay at
   * `presentation="tile"`, and a background tile whose Reader was already open
   * keeps an `OutputReader` mounted behind the scrim with its own `window`
   * `keydown` listener. Two listeners on one target both answer one `Escape` —
   * `stopPropagation` does nothing between them — so opening the Reader on tile
   * A and then the pane on tile B made one `Escape` close both. Dismissing a
   * Reader nobody can see costs the user nothing and is what makes
   * `DeckShell`'s "exactly one listener" true rather than documented-as-false.
   */
  panePresent?: boolean;
  /**
   * PRD #1105 M2 — close the pane, the mirror of {@link AgentTileProps.onOpen}.
   *
   * Deliberately NOT derived from `presentation`: closing acts on the VIEW and
   * this component holds no view state, so "is this rendered as an overlay" and
   * "can this be closed" are two facts and only one of them is a presentation.
   * The wrapper that positions the tile is what supplies it.
   */
  onClose?: () => void;
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
  noTerminal,
  recordFreshness = "live",
  terminalFocusToken,
  onSelect,
  onTabChange,
  onTerminalInput,
  onTerminalResize,
  appliedGeometry,
  onEvidenceSelect,
  onRename,
  onOpen,
  panePresent,
  onClose,
}: AgentTileProps) {
  const handleInput = useCallback((data: string) => {
    void onTerminalInput({ deckId: agent.daemonId, agentId: agent.id }, data);
  }, [agent.daemonId, agent.id, onTerminalInput]);
  const handleResize = useCallback((cols: number, rows: number) => {
    void onTerminalResize({ deckId: agent.daemonId, agentId: agent.id }, cols, rows);
  }, [agent.daemonId, agent.id, onTerminalResize]);
  const agentEvidence = evidence.filter((item) => agent.handoffIds.includes(item.id) || item.agentId === agent.id);
  // Issue #1042: one derivation for the terminal's state, its read-only gate
  // and the sentence beside it, so the three cannot disagree the way the tile's
  // inline status condition and the composer's own copy of it could.
  const input = terminalInputState(agent, inputResult);
  const fixture = mode === "fixture";
  /**
   * PRD #1105 M1 difference 4. The Reader launcher is `"tile"`-only, and
   * hiding the BUTTON is not enough: `readerOpen` is reset only when
   * `agent.id` changes, so a user with the Reader open on this tile who then
   * enlarges this same agent would carry `true` across and mount an
   * `OutputReader` inside the overlay — whose `window` `keydown` listener and
   * the overlay's would both answer one `Escape`, closing the Reader AND the
   * pane behind it. So the render is gated too, and the state is simply
   * unreachable at overlay presentation.
   *
   * That closes the SAME-tile route and no other; the cross-tile one is
   * `panePresent`, folded in at `readerSuppressed` below.
   */
  const overlay = presentation === "overlay";
  /* Issue #1143 — read once, so the header's two live-colour cues and its
     wording cannot disagree about the same record. */
  const held = recordFreshness === "held";
  /**
   * The Reader is suppressed at overlay presentation (M1 difference 4, above)
   * AND whenever any pane is open over this screen (see `panePresent`). The
   * second term is the cross-tile half the first cannot express: it is a fact
   * about the screen, not about this tile's own box.
   *
   * Gating the render as well as the state is deliberate. The effect below
   * dismisses an open Reader, but effects run after the commit, so for one
   * commit both listeners would still be bound without this.
   */
  const readerSuppressed = overlay || Boolean(panePresent);
  const [renameDraft, setRenameDraft] = useState<string>();
  const [readerOpen, setReaderOpen] = useState(false);
  useEffect(() => { setRenameDraft(undefined); setReaderOpen(false); }, [agent.id]);
  // Dismissed, not merely hidden: a Reader that came back when the pane closed
  // would answer the NEXT `Escape` instead of the deck, which is the same
  // surprise one step later.
  useEffect(() => { if (readerSuppressed) setReaderOpen(false); }, [readerSuppressed]);

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
      /*
        Issue #1143 — whether the record behind every field in this box is the
        deck's current answer or its last one. Deliberately a SIBLING of
        `data-status` rather than a value inside it: a `held` written into the
        status vocabulary would have to be read by everything that keys on a
        status, and the held record still HAS a status — it is simply one that
        was true earlier. Two facts, two attributes.
      */
      data-agent-record={recordFreshness}
      /* The seam the presentation differences key off. An attribute selector
         outranks a bare class even inside a media query, so
         `[data-presentation="overlay"]` can override the tile's own responsive
         rules with no stylesheet reordering. Nothing matches it yet. */
      data-presentation={presentation}
      onMouseDown={onSelect}
    >
      <header className="agent-header">
        <div className="agent-identity">
          <span className={`agent-state-mark status-${agent.status}${held ? " is-held" : ""}`} aria-hidden="true" />
          <div>
            <div className="agent-title-line">
              <h2>{agent.role}</h2>
              {agent.isStartRole && <span className="coordinator-badge" title="Orchestration start role">COORDINATOR</span>}
              {/*
                Issue #1143 — a held record's status is a past reading, so it is
                worded as one, and the live colour goes with it because a teal
                dot says "now" louder than any label can unsay.

                `last seen: <status>` rather than `last seen <status>`: the
                label-and-value reading is grammatical for every value the field
                takes, and half of them are not verbs — `last seen passed` and
                `last seen queued` do not parse, where `last seen: passed` does.
              */}
              <span className={`status-label status-${agent.status}${held ? " is-held" : ""}`}>{held ? `last seen: ${agent.status}` : agent.status}</span>
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
        <div className="agent-header-actions">
          <div className="agent-attempt" title={agent.attempt === undefined ? "No attempt count is reported by the deck" : "Current attempt"}>
            <span>ATT</span>
            <strong>{agent.attempt === undefined ? "—" : agent.attempt.toString().padStart(2, "0")}</strong>
          </div>
          {/*
            PRD #1105 M5's deck entry point, and M2's way back out. Both are
            native `<button>`s in the header rather than a click handler on the
            article, because the article already owns `onMouseDown` for
            selection and a whole-tile activation would have no keyboard
            equivalent, no role and no accessible name. The name carries the
            agent so a deck of nine reads as nine distinct controls.
          */}
          {onOpen && (
            <button
              className="agent-pane-control"
              aria-label={`Open ${agent.role} agent`}
              title={`Open ${agent.displayName} in a full-window pane`}
              onMouseDown={(event) => event.stopPropagation()}
              onClick={onOpen}
            ><Maximize2 size={13} /></button>
          )}
          {onClose && (
            <button
              className="agent-pane-control"
              aria-label={`Close ${agent.role} agent`}
              title={`Close ${agent.displayName} and go back`}
              onMouseDown={(event) => event.stopPropagation()}
              onClick={onClose}
            ><X size={14} /></button>
          )}
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
        {!readerSuppressed && (
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
        )}
      </div>

      {!readerSuppressed && readerOpen && <OutputReader agent={agent} onClose={() => setReaderOpen(false)} />}

      <div className="agent-panel" role="tabpanel">
        {tab === "terminal" && (
          /*
            PRD #1105 — `data-terminal-state` is the sibling of the viewport's
            own `data-input-state`, and it answers the outer question: is there
            a terminal here at all. `"attached"` is written positively rather
            than left absent so both directions can be asserted, and so a
            reader of the DOM never has to infer a state from a missing
            attribute.
          */
          <div className="agent-terminal-stack" data-terminal-state={noTerminal?.reason ?? "attached"}>
            {noTerminal ? (
              /*
                No `TerminalViewport` — this is the whole point rather than a
                saving. One mounted here would allocate an xterm and a WebGL
                context to render nothing, and what the user would read off that
                black rectangle is a claim about the agent. The box carries the
                sentence itself and is its own live region: there is no terminal
                beside it for the sentence to be beside.
              */
              <div
                className="panel-empty terminal-absent"
                data-testid={`terminal-absent-${agent.id}`}
                role="status"
                /* The exact instant behind the sentence's relative age, the
                   same pairing every other relativised instant in this app
                   uses (`ActivityDisplay`). Absent where there is no held
                   record to date. */
                title={noTerminal.noticeTitle}
              >
                <Unplug size={15} aria-hidden="true" />
                <span>{noTerminal.notice}</span>
              </div>
            ) : (
              <>
                <TerminalViewport
                  agentId={agent.id}
                  /* The agent's OWN deck, never the selected one: the feed is keyed
                     by the composite identity because ids collide across decks. */
                  deckId={agent.daemonId}
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
              </>
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
