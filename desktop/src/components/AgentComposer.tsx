import { useEffect, useRef, useState } from "react";
import { AlertTriangle, BookMarked, CheckCircle2, Send, XCircle } from "lucide-react";
import { isDelivered, sendResultReason } from "../types";
import type { AgentSession, DeckActionResult, DeckPrompt } from "../types";

/** Matches the Rust `COMMAND_MAX_BYTES` bound on submitted text. */
export const SUBMIT_TEXT_MAX_BYTES = 64 * 1024;

const encoder = new TextEncoder();

type Delivery =
  | { state: "idle" }
  | { state: "sending" }
  | { state: "delivered"; label: string }
  | { state: "failed"; detail: string };

interface AgentComposerProps {
  agent: AgentSession;
  prompts: DeckPrompt[];
  /** Increments when the command palette asks this composer to take focus. */
  focusToken?: number;
  onSubmit: (agentId: string, text: string) => Promise<DeckActionResult>;
}

/**
 * The agent's read-only gate: an agent that cannot take terminal input cannot
 * take submitted text either. Kept identical to the `TerminalViewport`
 * `readOnly` condition so the two controls never disagree.
 */
export function composerDisabledReason(agent: AgentSession): string | undefined {
  if (agent.status === "queued") return "This agent has not started yet.";
  if (agent.status === "passed") return "This agent has finished its work.";
  if (agent.status === "stopped") return "This agent is stopped.";
  return undefined;
}

export function AgentComposer({ agent, prompts, focusToken = 0, onSubmit }: AgentComposerProps) {
  const [text, setText] = useState("");
  const [delivery, setDelivery] = useState<Delivery>({ state: "idle" });
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const disabledReason = composerDisabledReason(agent);
  const bytes = encoder.encode(text).byteLength;
  const oversized = bytes > SUBMIT_TEXT_MAX_BYTES;
  const coordinator = agent.isStartRole === true;
  const ephemeralWorker = agent.inOrchestration === true && !coordinator;

  useEffect(() => {
    if (focusToken > 0) inputRef.current?.focus();
  }, [focusToken]);

  const submit = async () => {
    const payload = text.trim();
    if (!payload || oversized || disabledReason || delivery.state === "sending") return;
    setDelivery({ state: "sending" });
    try {
      const result = await onSubmit(agent.id, payload);
      if (isDelivered(result)) {
        setText("");
        setDelivery({ state: "delivered", label: result.sendResult === "queued" ? "Queued for the agent" : "Delivered to the agent" });
        return;
      }
      // A non-delivered outcome keeps the text in the box: it may not have
      // reached the agent, and retyping it is worse than retrying it.
      setDelivery({ state: "failed", detail: result.message ?? `Not delivered — ${sendResultReason(result.sendResult)}.` });
    } catch (cause) {
      setDelivery({ state: "failed", detail: cause instanceof Error ? cause.message : String(cause) });
    }
  };

  return (
    <div className={`agent-composer ${coordinator ? "is-coordinator" : ""}`} data-testid={`composer-${agent.id}`} onMouseDown={(event) => event.stopPropagation()}>
      <div className="composer-head">
        <span className="composer-title">{coordinator ? "Message coordinator" : `Message ${agent.role}`}</span>
        {prompts.length > 0 && (
          <label className="composer-prompt-picker">
            <BookMarked size={12} aria-hidden="true" />
            <select
              aria-label={`Insert saved prompt into ${agent.role} message`}
              value=""
              onChange={(event) => {
                const prompt = prompts.find((candidate) => candidate.id === event.target.value);
                if (!prompt) return;
                setText(prompt.body);
                setDelivery({ state: "idle" });
                inputRef.current?.focus();
              }}
            >
              <option value="">Insert prompt…</option>
              {prompts.map((prompt) => <option key={prompt.id} value={prompt.id}>{prompt.name || "Untitled prompt"}</option>)}
            </select>
          </label>
        )}
      </div>

      <textarea
        ref={inputRef}
        aria-label={`Message ${agent.role}`}
        rows={coordinator ? 3 : 2}
        value={text}
        disabled={Boolean(disabledReason)}
        title={disabledReason}
        placeholder={disabledReason ?? "Type a message… Enter sends, Shift+Enter adds a line."}
        onChange={(event) => {
          setText(event.target.value);
          if (delivery.state !== "sending") setDelivery({ state: "idle" });
        }}
        onKeyDown={(event) => {
          if (event.key !== "Enter" || event.shiftKey) return;
          event.preventDefault();
          void submit();
        }}
      />

      <div className="composer-actions">
        {ephemeralWorker && (
          <span className="composer-caution" data-testid={`composer-caution-${agent.id}`}>
            <AlertTriangle size={11} aria-hidden="true" />
            Non-coordinator roles may be respawned per delegation, so messages to them can be ephemeral. Prefer messaging the coordinator.
          </span>
        )}
        <span className="composer-spacer" />
        {oversized && <span className="composer-error"><AlertTriangle size={11} aria-hidden="true" /> Message is {(bytes / 1024).toFixed(1)} KiB — trim it below the 64 KiB limit.</span>}
        <button
          className="button primary compact"
          data-testid={`composer-send-${agent.id}`}
          disabled={Boolean(disabledReason) || oversized || !text.trim() || delivery.state === "sending"}
          title={disabledReason ?? "Send to this agent"}
          onClick={() => void submit()}
        ><Send size={13} aria-hidden="true" /> {delivery.state === "sending" ? "Sending…" : "Send"}</button>
      </div>

      {delivery.state === "delivered" && (
        <p className="composer-status is-delivered" role="status"><CheckCircle2 size={12} aria-hidden="true" /> {delivery.label}</p>
      )}
      {delivery.state === "failed" && (
        <p className="composer-status is-failed" role="alert"><XCircle size={12} aria-hidden="true" /> {delivery.detail}</p>
      )}
    </div>
  );
}
