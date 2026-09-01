import { useCallback, useEffect, useState } from "react";
import {
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
import type { AgentSession, DeckActionResult, DeckPrompt, EvidenceItem, PanelTab, RuntimeMode, TerminalFeed } from "../types";
import { AgentComposer } from "./AgentComposer";
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
  mode: RuntimeMode;
  selected: boolean;
  tab: PanelTab;
  terminalFeed?: TerminalFeed;
  evidence: EvidenceItem[];
  prompts: DeckPrompt[];
  /** Increments when the command palette asks this tile's composer to focus. */
  composerFocusToken?: number;
  onSelect: () => void;
  onTabChange: (tab: PanelTab) => void;
  onTerminalInput: (agentId: string, data: string) => Promise<void>;
  onTerminalResize: (agentId: string, cols: number, rows: number) => Promise<void>;
  onEvidenceSelect: (id: string) => void;
  onSubmitText: (agentId: string, text: string) => Promise<DeckActionResult>;
  onRename?: (agentId: string, displayName: string) => Promise<void>;
}

function formatTokens(tokens: number): string {
  return tokens >= 1_000 ? `${(tokens / 1_000).toFixed(1)}k` : String(tokens);
}

export function AgentTile({
  agent,
  mode,
  selected,
  tab,
  terminalFeed,
  evidence,
  prompts,
  composerFocusToken,
  onSelect,
  onTabChange,
  onTerminalInput,
  onTerminalResize,
  onEvidenceSelect,
  onSubmitText,
  onRename,
}: AgentTileProps) {
  const handleInput = useCallback((data: string) => {
    void onTerminalInput(agent.id, data);
  }, [agent.id, onTerminalInput]);
  const handleResize = useCallback((cols: number, rows: number) => {
    void onTerminalResize(agent.id, cols, rows);
  }, [agent.id, onTerminalResize]);
  const agentEvidence = evidence.filter((item) => agent.handoffIds.includes(item.id) || item.agentId === agent.id);
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
                {agent.cli} <span aria-hidden="true">·</span> {agent.model}
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
        <div className="agent-attempt" title={agent.attempt === undefined ? "No attempt count is reported by the daemon" : "Current attempt"}>
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

      {readerOpen && (
        <OutputReader agent={agent} prompts={prompts} onSubmit={onSubmitText} onClose={() => setReaderOpen(false)} />
      )}

      <div className="agent-panel" role="tabpanel">
        {tab === "terminal" && (
          <div className="agent-terminal-stack">
            <TerminalViewport
              agentId={agent.id}
              label={agent.role}
              transcript={agent.transcript}
              terminalFeed={terminalFeed}
              readOnly={agent.status === "queued" || agent.status === "passed" || agent.status === "stopped"}
              onInput={handleInput}
              onResize={handleResize}
              onFocus={onSelect}
            />
            <AgentComposer agent={agent} prompts={prompts} focusToken={composerFocusToken} onSubmit={onSubmitText} />
          </div>
        )}
        {tab === "diff" && (
          <div className="text-panel diff-panel">
            <div className="panel-caption"><FileCode2 size={14} /> Changed files</div>
            {agent.diff.length ? agent.diff.map((line) => <code key={line}>{line}</code>) : <EmptyPanel label={fixture ? "No changes in this agent's lease" : "Diff data is not exposed by the daemon"} />}
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
            )) : <EmptyPanel label={fixture ? "No checks attached" : "Check results are not exposed by the daemon"} />}
          </div>
        )}
        {tab === "handoffs" && (
          <div className="text-panel handoff-panel">
            {agentEvidence.length ? agentEvidence.map((item) => (
              <button key={item.id} onClick={() => onEvidenceSelect(item.id)}>
                <span className={`verdict verdict-${item.verdict.toLowerCase()}`}>{item.verdict}</span>
                <div><strong>{item.title}</strong><small>{item.from} → {item.to}</small></div>
              </button>
            )) : <EmptyPanel label={fixture ? "No handoff evidence yet" : "Structured handoffs are not exposed by the daemon"} />}
          </div>
        )}
        {tab === "artifacts" && (
          <div className="text-panel artifact-panel">
            {agent.artifacts.length ? agent.artifacts.map((artifact) => (
              <div key={artifact.id}><Box size={14} /><span><strong>{artifact.name}</strong><code>{artifact.path}</code></span></div>
            )) : <EmptyPanel label={fixture ? "No artifacts produced" : "Artifacts are not exposed by the daemon"} />}
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
